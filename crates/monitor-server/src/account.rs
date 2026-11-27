//! Account lifecycle (D-027): an account changes its own password, deletes itself, and
//! a workspace's owner takes the workspace's data away as one file.
//!
//! The two account routes are for the person behind an account session and nobody
//! else: a workspace API token and the operator session get 403. Both ask for the
//! account's password again, and that check is a credential guess like any sign-in: it
//! draws on the same per-address budget and the same password-work limit.

use std::time::Duration;

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use uuid::Uuid;

use monitor_db::{DailyStatsRow, MonitorRow, QueryError};
use monitor_domain::{AccountId, MonitorId, WorkspaceId};

use crate::{
    ApiError, AppState, CheckView, IncidentView, MaintenanceView, MonitorView, WRONG_TOKEN_DELAY,
    WritePace, auth, throttle,
};

/// Raw checks one monitor contributes to an export: its newest 5,000. A monitor with
/// more says so (`truncated`).
pub const EXPORT_CHECKS_PER_MONITOR: usize = 5_000;
/// Raw checks one export carries in all. Ten monitors at the per-monitor cap fit; past
/// it, later monitors get what is left and say `truncated`. This is what bounds the
/// response and the memory one export may take, whatever the workspace's plan.
pub const EXPORT_CHECKS_TOTAL: usize = 50_000;
/// Incidents, and maintenance windows, one export carries: the newest of each.
pub const EXPORT_ROWS: usize = 5_000;
/// Daily aggregate rows one export carries: the newest days first.
pub const EXPORT_DAILY_ROWS: usize = 40_000;
/// One export per workspace this often. An export reads a workspace's whole record
/// from a store with one writer; a loop of them must not be free.
pub const EXPORT_EVERY: Duration = Duration::from_secs(10);
/// Raw checks a workspace may hold when it is deleted with its only member's account.
/// The deletion is one transaction on a store with one writer; this is what keeps it
/// short. A free-plan workspace at its limits stays well under it (ten monitors, one
/// check a minute, kept two days); a larger one is told to delete monitors first.
pub const DELETE_MAX_CHECKS: i64 = 50_000;
/// Workspaces whose export cooldown is tracked at once; spent ones are forgotten past it.
const EXPORT_PACE_CAP: usize = 4096;

/// The per-workspace export cooldowns, `workspace -> (window opened, exports spent)`.
/// An extension `routes()` installs, like the credential guard.
#[derive(Clone, Default)]
pub struct ExportPace(WritePace);

/// `{ current_password, new_password }`. No `Debug`: a body that holds passwords is
/// never formatted into a log line.
#[derive(Deserialize)]
pub struct PasswordBody {
    pub current_password: String,
    pub new_password: String,
}

/// `{ password }`, the confirmation an account deletion asks for.
#[derive(Deserialize)]
pub struct DeleteAccountBody {
    pub password: String,
}

/// The account behind this request. Only an account session is one: a token is a
/// workspace's credential and the operator has no account.
fn account_session(principal: &auth::Principal) -> Result<AccountId, ApiError> {
    match principal {
        auth::Principal::Account { account, .. } => Ok(*account),
        _ => Err(ApiError::Forbidden(
            "only a signed-in account can change its password or delete itself".into(),
        )),
    }
}

/// Confirm the signed-in account's password, exactly as a sign-in would test it: the
/// address pays one guess before anything is compared (429 when it has none left), the
/// verification runs under the password-work limit (503 when the queue is full), a
/// wrong password keeps the guess spent and answers the sign-in's own 401, and a right
/// one gives the guess back.
async fn confirm_password(
    state: &AppState,
    extensions: &axum::http::Extensions,
    headers: &HeaderMap,
    account: AccountId,
    password: &str,
) -> Result<(), Box<Response>> {
    let Some(guard) = extensions.get::<throttle::Guard>() else {
        tracing::error!("invariant: routes() installs the credential guard");
        return Err(Box::new(ApiError::Corrupt.into_response()));
    };
    let attempt = guard
        .throttle
        .admit(guard.client(extensions, headers))
        .map_err(|wait| Box::new(throttle::too_many(wait)))?;
    let stored = state
        .db
        .account_password_hash(account)
        .await
        .map_err(|error| Box::new(ApiError::Db(error).into_response()))?;
    // An account that vanished mid-request verifies against the dummy hash: same
    // work, same answer as a wrong password.
    let hash = stored.as_deref().unwrap_or(auth::DUMMY_LOGIN_HASH);
    match auth::verify_password(hash, password, state.password_work_wait).await {
        Ok(true) if stored.is_some() => {
            attempt.succeeded();
            Ok(())
        }
        Ok(_) => {
            drop(attempt); // stays spent
            if let Ok(_penalty) = guard.penalties.try_acquire() {
                tokio::time::sleep(WRONG_TOKEN_DELAY).await;
            }
            Err(Box::new(crate::invalid_credentials()))
        }
        Err(auth::PasswordWorkError::Busy) => {
            drop(attempt);
            Err(Box::new(crate::password_busy()))
        }
        Err(auth::PasswordWorkError::Hash(error)) => {
            tracing::error!(%error, "password verification failed to run");
            Err(Box::new(ApiError::Corrupt.into_response()))
        }
    }
}

/// `POST /api/account/password`: change the signed-in account's password.
///
/// The new password follows the sign-up rule. On success every other session of the
/// account ends in the same transaction that stores the new hash; the calling session
/// stays, so the person who just proved the old password is not signed out.
pub(crate) async fn change_password(
    State(state): State<AppState>,
    axum::Extension(principal): axum::Extension<auth::Principal>,
    extensions: axum::http::Extensions,
    headers: HeaderMap,
    Json(body): Json<PasswordBody>,
) -> Response {
    let account = match account_session(&principal) {
        Ok(account) => account,
        Err(refusal) => return refusal.into_response(),
    };
    // Shape first: a malformed request is not a credential guess and spends nothing.
    if !crate::password_fits(&body.new_password) {
        return ApiError::Invalid("`new_password` must be 8..=256 characters".into())
            .into_response();
    }
    let Some(cookie) =
        auth::session_cookie(headers.get(header::COOKIE), state.cookie_secure.name())
    else {
        tracing::error!("invariant: an account principal arrives by session cookie");
        return ApiError::Corrupt.into_response();
    };
    let keep = auth::account_session_key(cookie);
    if let Err(refusal) = confirm_password(
        &state,
        &extensions,
        &headers,
        account,
        &body.current_password,
    )
    .await
    {
        return *refusal;
    }
    let hash = match auth::hash_password(&body.new_password, state.password_work_wait).await {
        Ok(hash) => hash,
        Err(auth::PasswordWorkError::Busy) => return crate::password_busy(),
        Err(auth::PasswordWorkError::Hash(error)) => {
            tracing::error!(%error, "password hashing failed");
            return ApiError::Corrupt.into_response();
        }
    };
    match state.db.set_account_password(account, &hash, &keep).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        // The account was deleted while this request ran: nothing to sign in to.
        Ok(false) => crate::invalid_credentials(),
        Err(error) => ApiError::Db(error).into_response(),
    }
}

/// `DELETE /api/account`: delete the signed-in account (D-027).
///
/// The store does the whole removal in one transaction, or refuses while the account
/// is the only owner of a workspace other people are in. Afterwards the things held
/// outside the store follow: the schedule stops probing the deleted workspaces'
/// monitors, their public status pages stop resolving and their files go, and the
/// response expires the session cookie.
pub(crate) async fn delete_account(
    State(state): State<AppState>,
    axum::Extension(principal): axum::Extension<auth::Principal>,
    extensions: axum::http::Extensions,
    headers: HeaderMap,
    Json(body): Json<DeleteAccountBody>,
) -> Response {
    let account = match account_session(&principal) {
        Ok(account) => account,
        Err(refusal) => return refusal.into_response(),
    };
    if let Err(refusal) =
        confirm_password(&state, &extensions, &headers, account, &body.password).await
    {
        return *refusal;
    }
    // The deletion and what follows it run to the end even if this request is dropped
    // (the client hung up, the deadline passed): a committed deletion whose monitors
    // stayed on the schedule would keep probing targets nobody watches any more. The
    // task is short (one store transaction, a few file removals, one registry read)
    // and it is awaited here.
    let work = tokio::spawn(delete_and_forget(state.clone(), account));
    match work.await {
        Ok(Ok(())) => {}
        Ok(Err(QueryError::Conflict(message))) => {
            return ApiError::Conflict(message).into_response();
        }
        Ok(Err(error)) => return ApiError::Db(error).into_response(),
        Err(join) => {
            tracing::error!(error = %join, "account deletion task did not finish");
            return ApiError::Corrupt.into_response();
        }
    }
    (
        StatusCode::NO_CONTENT,
        [(
            header::SET_COOKIE,
            crate::expired_session_cookie(&state, &headers),
        )],
    )
        .into_response()
}

/// The store's deletion, then everything held outside the store that must follow it.
async fn delete_and_forget(state: AppState, account: AccountId) -> Result<(), QueryError> {
    let gone = state.db.delete_account(account, DELETE_MAX_CHECKS).await?;
    forget_workspaces(&state, &gone).await;
    // The account is gone whatever happens next, so a registry that could not be
    // re-read is logged, not answered: until the next schedule write the coordinator
    // probes monitors that no longer exist, and the writer drops their results.
    if let Err(error) = crate::publish_registry(&state).await {
        tracing::error!(%error, "registry not republished after an account deletion");
    }
    Ok(())
}

/// Drop what the server holds about deleted workspaces outside the store: their slugs
/// in the public route's cache (the version bump also retires an entry a reader raced
/// in) and their snapshot files. The snapshot writer is woken so the next beat agrees.
async fn forget_workspaces(state: &AppState, gone: &[WorkspaceId]) {
    if gone.is_empty() {
        return;
    }
    state
        .status_slug_version
        .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    {
        let mut slugs = state
            .status_slugs
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        slugs.retain(|_, entry| entry.workspace.is_none_or(|ws| !gone.contains(&ws)));
    }
    for workspace in gone {
        let file = state
            .status_dir
            .join("s")
            .join(format!("{}.json", workspace.0));
        // Absent is the usual case: most workspaces never published a page.
        let _ = tokio::fs::remove_file(file).await;
    }
    state.status_changed.notify_one();
}

/// The 429 a workspace gets for a second export inside [`EXPORT_EVERY`].
fn export_too_soon(wait: Duration) -> Response {
    (
        StatusCode::TOO_MANY_REQUESTS,
        [(
            header::RETRY_AFTER,
            throttle::retry_after_secs(wait).to_string(),
        )],
        Json(serde_json::json!({
            "error": { "code": "too_many_requests",
                       "message": "one export every 10 seconds per workspace; retry shortly" }
        })),
    )
        .into_response()
}

/// One monitor's raw checks for the export: its newest rows, within the per-monitor
/// cap and what is left of the export's total. `truncated` is true when the monitor
/// has more checks than were written.
async fn export_checks(
    view: &monitor_db::OwnerDb<'_>,
    monitor: &MonitorRow,
    budget: &mut usize,
) -> Result<serde_json::Value, ApiError> {
    let id = Uuid::parse_str(&monitor.id)
        .map(MonitorId)
        .map_err(|_| ApiError::Corrupt)?;
    let take = EXPORT_CHECKS_PER_MONITOR.min(*budget);
    // One row past the cap answers "is there more?" without a second query.
    let ask = i64::try_from(take + 1).unwrap_or(i64::MAX);
    let mut rows = view.list_checks(id, ask).await?;
    let truncated = rows.len() > take;
    rows.truncate(take);
    *budget -= rows.len();
    let rows: Vec<CheckView> = rows.into_iter().map(CheckView::from).collect();
    Ok(serde_json::json!({
        "monitor_id": monitor.id,
        "truncated": truncated,
        "rows": rows,
    }))
}

/// Keep at most `cap` of rows that were asked for with one to spare: the flag says
/// whether the workspace holds more than the export carries.
fn capped<T>(mut rows: Vec<T>, cap: usize) -> (Vec<T>, bool) {
    let truncated = rows.len() > cap;
    rows.truncate(cap);
    (rows, truncated)
}

fn daily_view(row: &DailyStatsRow) -> serde_json::Value {
    serde_json::json!({
        "monitor_id": row.monitor_id,
        "day": row.day,
        "pass": row.pass,
        "http_fail": row.http_fail,
        "transport": row.transport,
        "timeout": row.timeout,
        "missed": row.missed,
    })
}

/// `GET /api/workspace/export`: the caller's workspace as one JSON document. Owner
/// work (the operator included); a workspace API token never holds owner, so a token
/// gets 403 whatever role it was minted with.
///
/// What it carries: the workspace (id, name, slug, plan), the roster as email and
/// role, the monitors as the API shows them, maintenance windows, incidents, daily
/// aggregates and each monitor's newest raw checks. What it never carries: the webhook
/// URL or its signing secret, API tokens or their digests, password hashes, session
/// rows, invitations, or a row of any other workspace: every read goes through this
/// workspace's own view of the store.
pub(crate) async fn export_workspace(
    State(state): State<AppState>,
    axum::Extension(principal): axum::Extension<auth::Principal>,
    axum::Extension(pace): axum::Extension<ExportPace>,
) -> Result<Response, ApiError> {
    let view = crate::owner_for(&state, &principal)
        .map_err(|_| ApiError::Forbidden("exporting a workspace's data is owner work".into()))?;
    if let Err(wait) = crate::spend_write_budget(
        &pace.0,
        principal.workspace(),
        1,
        EXPORT_EVERY,
        EXPORT_PACE_CAP,
    ) {
        return Ok(export_too_soon(wait));
    }
    let workspace = state
        .db
        .workspace_row(principal.workspace())
        .await?
        .ok_or(ApiError::NotFound)?;
    let now = (state.clock)();
    let observing = state.vitals.verdict().observing();

    let mut budget = EXPORT_CHECKS_TOTAL;
    let mut monitors: Vec<MonitorView> = Vec::new();
    let mut checks = Vec::new();
    for row in view.list_monitors().await? {
        checks.push(export_checks(&view, &row, &mut budget).await?);
        monitors.push(crate::view_from_row(row, &state.statuses, observing)?);
    }
    let members = view
        .list_members()
        .await?
        .iter()
        .map(|member| serde_json::json!({ "email": member.email, "role": member.role }))
        .collect::<Vec<_>>();
    let ask = |cap: usize| i64::try_from(cap + 1).unwrap_or(i64::MAX);
    let (incidents, more_incidents) = capped(
        view.list_incidents(None, None, ask(EXPORT_ROWS)).await?,
        EXPORT_ROWS,
    );
    let (windows, more_windows) = capped(view.export_windows(ask(EXPORT_ROWS)).await?, EXPORT_ROWS);
    let (daily, more_daily) = capped(
        view.export_daily_stats(ask(EXPORT_DAILY_ROWS)).await?,
        EXPORT_DAILY_ROWS,
    );
    let body = serde_json::json!({
        "exported_at": crate::stamp(now)?,
        "workspace": {
            "id": workspace.id, "name": workspace.name,
            "slug": workspace.slug, "plan": workspace.plan,
        },
        "members": members,
        "monitors": monitors,
        "maintenance_windows": windows
            .into_iter()
            .map(|row| MaintenanceView::from_row(row, now))
            .collect::<Vec<_>>(),
        "incidents": incidents.into_iter().map(IncidentView::from).collect::<Vec<_>>(),
        "daily_aggregates": daily.iter().map(daily_view).collect::<Vec<_>>(),
        "checks": checks,
        "truncated": {
            "incidents": more_incidents,
            "maintenance_windows": more_windows,
            "daily_aggregates": more_daily,
        },
    });
    Ok(attachment(&workspace.slug, body))
}

/// The export as a download: named after the workspace's slug, and never cached.
fn attachment(slug: &str, body: serde_json::Value) -> Response {
    // A slug is lowercase letters, digits and hyphens wherever the API wrote it; a
    // stored one that is not (an older seeded row) must not reach a header as is.
    let name = if crate::valid_slug(slug) {
        slug
    } else {
        "workspace"
    };
    (
        [
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"service-monitor-export-{name}.json\""),
            ),
            (header::CACHE_CONTROL, "no-store".to_owned()),
        ],
        Json(body),
    )
        .into_response()
}
