//! Account lifecycle in the store (D-027), counted row by row.
//!
//! The API tests prove what a client sees; these prove what is left in the tables. A
//! second connection to the same file counts every tenant-owned table directly, so a
//! delete that misses a table — or reaches into another workspace's — shows up as a
//! number, whatever the store's own read methods would say.

use std::collections::HashMap;
use std::sync::Arc;

use monitor_db::{Db, NewAccount, Observation, QueryError, writer};
use monitor_domain::{
    AccountId, CheckOutcome, Monitor, NewMonitor, Role, SlotObservation, WorkspaceId,
};
use sqlx::SqlitePool;
use time::OffsetDateTime;

/// Every table that holds a workspace's rows, with the count of one workspace's.
const WORKSPACE_TABLES: [(&str, &str); 12] = [
    ("workspaces", "SELECT COUNT(*) FROM workspaces WHERE id = ?"),
    (
        "monitors",
        "SELECT COUNT(*) FROM monitors WHERE workspace_id = ?",
    ),
    (
        "checks",
        "SELECT COUNT(*) FROM checks WHERE workspace_id = ?",
    ),
    (
        "incidents",
        "SELECT COUNT(*) FROM incidents WHERE workspace_id = ?",
    ),
    (
        "notify_outbox",
        "SELECT COUNT(*) FROM notify_outbox WHERE workspace_id = ?",
    ),
    (
        "check_stats_daily",
        "SELECT COUNT(*) FROM check_stats_daily WHERE workspace_id = ?",
    ),
    (
        "maintenance_windows",
        "SELECT COUNT(*) FROM maintenance_windows WHERE workspace_id = ?",
    ),
    (
        "status_monitors",
        "SELECT COUNT(*) FROM status_monitors WHERE workspace_id = ?",
    ),
    (
        "status_held_slugs",
        "SELECT COUNT(*) FROM status_held_slugs WHERE workspace_id = ?",
    ),
    (
        "api_tokens",
        "SELECT COUNT(*) FROM api_tokens WHERE workspace_id = ?",
    ),
    (
        "workspace_invitations",
        "SELECT COUNT(*) FROM workspace_invitations WHERE workspace_id = ?",
    ),
    (
        "members",
        "SELECT COUNT(*) FROM members WHERE workspace_id = ?",
    ),
];

/// A check bound no fixture comes near: the tests that are not about the bound.
const ROOMY: i64 = 50_000;

struct Store {
    db: Db,
    /// A second connection to the same file, for counting what the store API hides.
    raw: SqlitePool,
    writer: writer::WriterHandle,
    _tmp: tempfile::TempDir,
}

async fn store() -> anyhow::Result<Store> {
    let tmp = tempfile::tempdir()?;
    let url = format!("sqlite:{}", tmp.path().join("account.db").display());
    let db = Db::connect(&url, 1).await?;
    db.migrate().await?;
    let raw = SqlitePool::connect(&url).await?;
    let (writer, _task) = writer::spawn(
        db.clone(),
        Arc::default(),
        HashMap::new(),
        Arc::default(),
        8,
        false,
    );
    Ok(Store {
        db,
        raw,
        writer,
        _tmp: tmp,
    })
}

/// How many rows `workspace` has in each tenant-owned table.
async fn counts(raw: &SqlitePool, workspace: WorkspaceId) -> anyhow::Result<Vec<(&str, i64)>> {
    let mut found = Vec::new();
    for (table, sql) in WORKSPACE_TABLES {
        let count: i64 = sqlx::query_scalar(sql)
            .bind(workspace.0.to_string())
            .fetch_one(raw)
            .await?;
        found.push((table, count));
    }
    Ok(found)
}

/// Sign an account up the way the API does: an invite, then the one transaction that
/// mints the workspace, the account and its owner seat.
async fn tenant(db: &Db, name: &str) -> anyhow::Result<(AccountId, WorkspaceId)> {
    let now = OffsetDateTime::now_utc();
    let invite = db
        .create_invite(
            &format!("digest-of-{name}"),
            now,
            now + time::Duration::hours(1),
        )
        .await?;
    let account = NewAccount {
        id: AccountId::new(),
        workspace: WorkspaceId::new(),
        workspace_slug: format!("ws-{name}"),
        workspace_name: format!("{name}'s workspace"),
        email: format!("{name}@tenant.example"),
        password_hash: format!("hash-of-{name}"),
    };
    db.signup(&invite.id, &account, now).await?;
    Ok((account.id, account.workspace))
}

async fn record(store: &Store, monitor: &Monitor, outcome: CheckOutcome) -> anyhow::Result<()> {
    store
        .writer
        .record(Observation {
            monitor_id: monitor.id,
            workspace: monitor.workspace,
            generation: monitor.generation,
            slot_at: OffsetDateTime::now_utc(),
            slot: SlotObservation::Probe(outcome),
            http_status: Some(500),
            latency_ms: Some(12),
            detail: None,
        })
        .await?;
    Ok(())
}

/// Put a row in every table a workspace can own: a monitor with checks, an open
/// incident and its queued alert, a daily aggregate, a maintenance window, a published
/// status page with a slug it gave up, an API token, a pending invitation.
async fn fill(store: &Store, owner: AccountId, workspace: WorkspaceId) -> anyhow::Result<Monitor> {
    let db = &store.db;
    let tag = workspace.0.simple().to_string()[20..].to_owned();
    let monitor = NewMonitor::new(
        format!("api-{tag}"),
        url::Url::parse("https://target.example/health")?,
    )?
    .into_monitor_for(workspace);
    db.workspace(workspace).insert_monitor(&monitor).await?;
    record(store, &monitor, CheckOutcome::Pass).await?;
    for _ in 0..3 {
        record(store, &monitor, CheckOutcome::HttpFail { status: 503 }).await?;
    }
    sqlx::query(
        "INSERT INTO check_stats_daily (monitor_id, workspace_id, day, pass, missed)
         VALUES (?, ?, '2026-01-01', 1400, 40)",
    )
    .bind(monitor.id.0.to_string())
    .bind(workspace.0.to_string())
    .execute(&store.raw)
    .await?;
    let now = OffsetDateTime::now_utc();
    let member = db.member(workspace);
    member
        .create_window(monitor.id, now, now + time::Duration::hours(1), "deploy")
        .await?;
    member
        .set_status_page(Some(&format!("first-{tag}")), Some("Status"), &[monitor.id])
        .await?;
    // Renaming the page leaves the old slugs held by this workspace.
    member
        .set_status_page(
            Some(&format!("second-{tag}")),
            Some("Status"),
            &[monitor.id],
        )
        .await?;
    let admin = db.owner(workspace);
    admin
        .create_api_token(
            "deploy",
            Role::Member,
            &format!("token-digest-{tag}"),
            Some(owner),
            now,
        )
        .await?;
    admin
        .invite_member(
            &format!("guest-{tag}@tenant.example"),
            Role::Viewer,
            "operator",
        )
        .await?;
    Ok(monitor)
}

async fn scalar(raw: &SqlitePool, sql: &'static str, bound: &str) -> anyhow::Result<i64> {
    Ok(sqlx::query_scalar(sql).bind(bound).fetch_one(raw).await?)
}

/// The core of D-027: one account's deletion empties every table of the workspace only
/// it could reach, and changes no count of a second account's workspace.
#[tokio::test]
async fn deleting_an_account_removes_its_sole_workspace_and_nothing_of_anyone_elses()
-> anyhow::Result<()> {
    let store = store().await?;
    let (leaving, gone) = tenant(&store.db, "leaving").await?;
    let (staying, kept) = tenant(&store.db, "staying").await?;
    fill(&store, leaving, gone).await?;
    fill(&store, staying, kept).await?;
    let now = OffsetDateTime::now_utc();
    let later = now + time::Duration::hours(1);
    for (key, account) in [("session-leaving", leaving), ("session-staying", staying)] {
        store
            .db
            .create_account_session_at(key, account, now, later)
            .await?;
    }
    // An invitation from the other workspace, addressed to the account that leaves.
    store
        .db
        .owner(kept)
        .invite_member("leaving@tenant.example", Role::Member, "operator")
        .await?;

    let before_gone = counts(&store.raw, gone).await?;
    assert!(
        before_gone.iter().all(|(_, count)| *count > 0),
        "the fixture leaves a table of the deleted workspace empty: {before_gone:?}"
    );
    let before_kept = counts(&store.raw, kept).await?;
    let default_before = counts(&store.raw, WorkspaceId::DEFAULT).await?;

    assert_eq!(
        store.db.delete_account(leaving, None, ROOMY).await?,
        vec![gone]
    );

    for (table, count) in counts(&store.raw, gone).await? {
        assert_eq!(count, 0, "{table} still holds the deleted workspace's rows");
    }
    // The other workspace lost exactly one row: the invitation addressed to the
    // account that no longer exists. Every other count is what it was.
    let mut expected = before_kept;
    for (table, count) in &mut expected {
        if *table == "workspace_invitations" {
            *count -= 1;
        }
    }
    assert_eq!(counts(&store.raw, kept).await?, expected);
    assert_eq!(
        counts(&store.raw, WorkspaceId::DEFAULT).await?,
        default_before
    );

    let leaving_id = leaving.0.to_string();
    for (what, sql) in [
        ("accounts", "SELECT COUNT(*) FROM accounts WHERE id = ?"),
        (
            "sessions",
            "SELECT COUNT(*) FROM sessions WHERE account_id = ?",
        ),
        (
            "members",
            "SELECT COUNT(*) FROM members WHERE account_id = ?",
        ),
        (
            "invites.used_by",
            "SELECT COUNT(*) FROM invites WHERE used_by = ?",
        ),
        (
            "api_tokens.created_by",
            "SELECT COUNT(*) FROM api_tokens WHERE created_by = ?",
        ),
    ] {
        assert_eq!(
            scalar(&store.raw, sql, &leaving_id).await?,
            0,
            "{what} still names the deleted account"
        );
    }
    // The signup invite stays spent: clearing who used it must not make it usable.
    assert_eq!(
        scalar(
            &store.raw,
            "SELECT COUNT(*) FROM invites WHERE code_digest = ? AND used_at IS NOT NULL",
            "digest-of-leaving"
        )
        .await?,
        1
    );
    // The other account still has its session and its row.
    assert_eq!(
        scalar(
            &store.raw,
            "SELECT COUNT(*) FROM sessions WHERE account_id = ?",
            &staying.0.to_string()
        )
        .await?,
        1
    );
    assert!(store.db.account_by_id(staying).await?.is_some());
    Ok(())
}

/// The refusal: the only owner of a workspace other people are in cannot leave them in
/// a workspace nobody can administer. Nothing changes, the sessions included: the
/// transaction's first statement deletes them, and the refusal must roll that back.
#[tokio::test]
async fn the_only_owner_of_a_shared_workspace_is_refused_and_nothing_changes() -> anyhow::Result<()>
{
    let store = store().await?;
    let (owner, shared) = tenant(&store.db, "owner").await?;
    let (guest, guests_own) = tenant(&store.db, "guest").await?;
    fill(&store, owner, shared).await?;
    let now = OffsetDateTime::now_utc();
    store
        .db
        .owner(shared)
        .add_member(guest, Role::Member, now)
        .await?;
    store
        .db
        .create_account_session_at("session-owner", owner, now, now + time::Duration::hours(1))
        .await?;
    let before = counts(&store.raw, shared).await?;

    let refusal = store.db.delete_account(owner, None, ROOMY).await;
    match refusal {
        Err(QueryError::Conflict(message)) => assert_eq!(
            message,
            "the workspace \"owner's workspace\" needs an owner: make another member an \
             owner, then delete your account"
        ),
        other => anyhow::bail!("expected the last-owner refusal, got {other:?}"),
    }
    assert_eq!(counts(&store.raw, shared).await?, before);
    assert!(store.db.account_by_id(owner).await?.is_some());
    assert_eq!(
        scalar(
            &store.raw,
            "SELECT COUNT(*) FROM sessions WHERE account_id = ?",
            &owner.0.to_string()
        )
        .await?,
        1,
        "a refused deletion must not sign the account out"
    );

    // Once someone else is an owner the same deletion goes through, and the shared
    // workspace survives with everything in it but the leaver's seat.
    assert!(
        store
            .db
            .owner(shared)
            .set_member_role(guest, Role::Owner)
            .await?
    );
    assert_eq!(
        store.db.delete_account(owner, None, ROOMY).await?,
        Vec::new()
    );
    let mut expected = before;
    for (table, count) in &mut expected {
        if *table == "members" {
            *count -= 1;
        }
    }
    assert_eq!(counts(&store.raw, shared).await?, expected);
    assert_eq!(
        store.db.member_role(shared, guest).await?,
        Some(Role::Owner)
    );
    assert!(store.db.workspace_row(guests_own).await?.is_some());
    Ok(())
}

/// A member who is not the last owner leaves a shared workspace whole, and takes only
/// their own workspace with them.
#[tokio::test]
async fn a_member_who_deletes_their_account_leaves_a_shared_workspace_whole() -> anyhow::Result<()>
{
    let store = store().await?;
    let (owner, shared) = tenant(&store.db, "owner").await?;
    let (member, members_own) = tenant(&store.db, "member").await?;
    fill(&store, owner, shared).await?;
    fill(&store, member, members_own).await?;
    store
        .db
        .owner(shared)
        .add_member(member, Role::Member, OffsetDateTime::now_utc())
        .await?;
    let before = counts(&store.raw, shared).await?;

    assert_eq!(
        store.db.delete_account(member, None, ROOMY).await?,
        vec![members_own]
    );

    let mut expected = before;
    for (table, count) in &mut expected {
        if *table == "members" {
            *count -= 1;
        }
    }
    assert_eq!(counts(&store.raw, shared).await?, expected);
    assert_eq!(store.db.member_role(shared, member).await?, None);
    assert_eq!(
        store.db.member_role(shared, owner).await?,
        Some(Role::Owner)
    );
    for (table, count) in counts(&store.raw, members_own).await? {
        assert_eq!(count, 0, "{table}");
    }
    Ok(())
}

/// A workspace whose founder is gone is no account's home, so no foreign key stands
/// between it and a delete: only the "only member" rule does. One of its two owners
/// deleting their account must leave it whole for the other.
#[tokio::test]
async fn a_workspace_nobody_calls_home_survives_one_of_its_owners_leaving() -> anyhow::Result<()> {
    let store = store().await?;
    let (creator, shared) = tenant(&store.db, "creator").await?;
    let (first, _) = tenant(&store.db, "first").await?;
    let (second, _) = tenant(&store.db, "second").await?;
    fill(&store, creator, shared).await?;
    let now = OffsetDateTime::now_utc();
    for owner in [first, second] {
        store
            .db
            .owner(shared)
            .add_member(owner, Role::Owner, now)
            .await?;
    }
    assert_eq!(
        store.db.delete_account(creator, None, ROOMY).await?,
        Vec::new()
    );
    let before = counts(&store.raw, shared).await?;

    let gone = store.db.delete_account(first, None, ROOMY).await?;

    assert!(
        !gone.contains(&shared),
        "a workspace another owner is still in was deleted"
    );
    let mut expected = before;
    for (table, count) in &mut expected {
        if *table == "members" {
            *count -= 1;
        }
    }
    assert_eq!(counts(&store.raw, shared).await?, expected);
    assert_eq!(
        store.db.member_role(shared, second).await?,
        Some(Role::Owner)
    );
    Ok(())
}

/// `accounts.workspace_id` has no cascade: a workspace that is another account's home
/// can only be deleted once that account points somewhere that exists. The other
/// account is moved, never deleted and never given a seat it did not have. Without the
/// move the deletion would fail on the foreign key and the heir could never leave.
#[tokio::test]
async fn an_account_whose_home_workspace_is_deleted_is_moved_not_broken() -> anyhow::Result<()> {
    let store = store().await?;
    let (founder, first_ws) = tenant(&store.db, "founder").await?;
    let (heir, heirs_own) = tenant(&store.db, "heir").await?;
    let (drifter, second_ws) = tenant(&store.db, "drifter").await?;
    let (_host, hosted) = tenant(&store.db, "host").await?;
    let now = OffsetDateTime::now_utc();
    // The heir takes over two workspaces whose founders are no longer in them. An
    // account can't be removed from its own workspace any more, so this is a state only
    // older rows hold: the seat is taken away directly, as it once could be.
    for (workspace, old) in [(first_ws, founder), (second_ws, drifter)] {
        store
            .db
            .owner(workspace)
            .add_member(heir, Role::Owner, now)
            .await?;
        let removed = sqlx::query("DELETE FROM members WHERE workspace_id = ? AND account_id = ?")
            .bind(workspace.0.to_string())
            .bind(old.0.to_string())
            .execute(&store.raw)
            .await?
            .rows_affected();
        assert_eq!(removed, 1);
    }
    // The founder has a seat elsewhere; the drifter has none anywhere.
    store
        .db
        .owner(hosted)
        .add_member(founder, Role::Viewer, now)
        .await?;

    let mut gone = store.db.delete_account(heir, None, ROOMY).await?;
    gone.sort_by_key(|workspace| workspace.0);
    let mut expected = vec![first_ws, second_ws, heirs_own];
    expected.sort_by_key(|workspace| workspace.0);
    assert_eq!(gone, expected);

    for (account, home, why) in [
        (founder, hosted, "moved to the workspace it has a seat in"),
        (
            drifter,
            WorkspaceId::DEFAULT,
            "parked on the seeded workspace, with no seat",
        ),
    ] {
        let row = store
            .db
            .account_by_id(account)
            .await?
            .ok_or_else(|| anyhow::anyhow!("the other account was deleted"))?;
        assert_eq!(row.workspace_id, home.0.to_string(), "{why}");
    }
    assert_eq!(
        store.db.member_role(hosted, founder).await?,
        Some(Role::Viewer)
    );
    assert_eq!(
        store.db.member_role(WorkspaceId::DEFAULT, drifter).await?,
        None,
        "moving an account's home must not seat it anywhere"
    );
    Ok(())
}

/// The seeded workspace is the operator's: an account that is its only member leaves
/// it, and what was recorded there stays.
#[tokio::test]
async fn the_seeded_workspace_is_never_deleted_with_an_account() -> anyhow::Result<()> {
    let store = store().await?;
    let (account, own) = tenant(&store.db, "seated").await?;
    store
        .db
        .owner(WorkspaceId::DEFAULT)
        .add_member(account, Role::Owner, OffsetDateTime::now_utc())
        .await?;
    let monitor = NewMonitor::new("operator's", url::Url::parse("https://ops.example/")?)?
        .into_monitor_for(WorkspaceId::DEFAULT);
    store
        .db
        .workspace(WorkspaceId::DEFAULT)
        .insert_monitor(&monitor)
        .await?;
    record(&store, &monitor, CheckOutcome::Pass).await?;
    let before = counts(&store.raw, WorkspaceId::DEFAULT).await?;

    assert_eq!(
        store.db.delete_account(account, None, ROOMY).await?,
        vec![own]
    );

    let mut expected = before;
    for (table, count) in &mut expected {
        if *table == "members" {
            *count -= 1;
        }
    }
    assert_eq!(counts(&store.raw, WorkspaceId::DEFAULT).await?, expected);
    Ok(())
}

/// The one transaction that removes a workspace holds the store's write lock, so it is
/// bounded: a workspace with more evidence rows than the bound is refused by name, with
/// nothing deleted and the account still signed in, and goes once it is under it.
#[tokio::test]
async fn a_workspace_with_too_much_history_is_refused_until_it_is_smaller() -> anyhow::Result<()> {
    let store = store().await?;
    let (account, workspace) = tenant(&store.db, "heavy").await?;
    fill(&store, account, workspace).await?;
    // Checks, incidents and queued alerts count together.
    let id = workspace.0.to_string();
    let checks = scalar(
        &store.raw,
        "SELECT COUNT(*) FROM checks WHERE workspace_id = ?",
        &id,
    )
    .await?;
    let incidents = scalar(
        &store.raw,
        "SELECT COUNT(*) FROM incidents WHERE workspace_id = ?",
        &id,
    )
    .await?;
    let alerts = scalar(
        &store.raw,
        "SELECT COUNT(*) FROM notify_outbox WHERE workspace_id = ?",
        &id,
    )
    .await?;
    assert!(
        checks > 0 && incidents > 0 && alerts > 0,
        "the fixture holds all three"
    );
    // The windows and tokens the same transaction cascades over count as well (the
    // test below is about those).
    let cascaded = scalar(
        &store.raw,
        "SELECT (SELECT COUNT(*) FROM maintenance_windows WHERE workspace_id = ?1)
              + (SELECT COUNT(*) FROM api_tokens WHERE workspace_id = ?1)
              + (SELECT COUNT(*) FROM workspace_invitations WHERE workspace_id = ?1)",
        &id,
    )
    .await?;
    let rows = checks + incidents + alerts + cascaded;
    let now = OffsetDateTime::now_utc();
    store
        .db
        .create_account_session_at(
            "session-heavy",
            account,
            now,
            now + time::Duration::hours(1),
        )
        .await?;
    let before = counts(&store.raw, workspace).await?;

    // A bound that covers the checks alone is not enough: the incident and its
    // alert are rows the same transaction has to remove.
    assert!(matches!(
        store.db.delete_account(account, None, checks).await,
        Err(QueryError::Conflict(_))
    ));
    match store.db.delete_account(account, None, rows - 1).await {
        Err(QueryError::Conflict(message)) => assert_eq!(
            message,
            "the workspace \"heavy's workspace\" holds too much history to delete in one \
             step: delete its monitors with their history, then delete your account"
        ),
        other => anyhow::bail!("expected the history-bound refusal, got {other:?}"),
    }
    assert_eq!(counts(&store.raw, workspace).await?, before);
    assert!(store.db.account_by_id(account).await?.is_some());
    assert_eq!(
        scalar(
            &store.raw,
            "SELECT COUNT(*) FROM sessions WHERE account_id = ?",
            &account.0.to_string()
        )
        .await?,
        1
    );

    // Exactly at the bound is allowed.
    assert_eq!(
        store.db.delete_account(account, None, rows).await?,
        vec![workspace]
    );
    Ok(())
}

/// A password change is one transaction: the new hash is stored and every session but
/// the caller's ends, another account's included in neither.
#[tokio::test]
async fn a_password_change_ends_every_other_session_of_that_account_only() -> anyhow::Result<()> {
    let store = store().await?;
    let (account, _) = tenant(&store.db, "changing").await?;
    let (other, _) = tenant(&store.db, "bystander").await?;
    let now = OffsetDateTime::now_utc();
    let later = now + time::Duration::hours(1);
    for (key, who) in [
        ("this-device", account),
        ("old-laptop", account),
        ("old-phone", account),
        ("bystander", other),
    ] {
        store
            .db
            .create_account_session_at(key, who, now, later)
            .await?;
    }
    let idle = std::time::Duration::from_secs(3600);

    let stored = store
        .db
        .account_password_hash(account)
        .await?
        .expect("the account has a password");
    assert!(
        store
            .db
            .set_account_password(account, &stored, "the-new-hash", "this-device")
            .await?
    );
    // A second change that verified the OLD hash arrives late: it must not overwrite
    // the password that was just set, nor end the session that set it.
    assert!(
        !store
            .db
            .set_account_password(account, &stored, "a-stale-writers-hash", "old-laptop")
            .await?,
        "a change that proved the previous password overwrote the new one"
    );

    assert_eq!(
        store.db.account_password_hash(account).await?.as_deref(),
        Some("the-new-hash")
    );
    assert_eq!(
        store.db.account_password_hash(other).await?.as_deref(),
        Some("hash-of-bystander")
    );
    for (key, alive) in [
        ("this-device", true),
        ("old-laptop", false),
        ("old-phone", false),
        ("bystander", true),
    ] {
        assert_eq!(
            store.db.account_session_at(key, now, idle).await?.is_some(),
            alive,
            "{key}"
        );
    }
    // An account that no longer exists changes nothing and says so.
    assert!(
        !store
            .db
            .set_account_password(AccountId::new(), "any", "x", "y")
            .await?
    );
    Ok(())
}

/// Sign-in verifies against a hash it read before the slow part. If the password
/// changed, or the account was deleted, while it verified, no session is created.
#[tokio::test]
async fn no_session_is_created_for_a_password_hash_that_is_no_longer_stored() -> anyhow::Result<()>
{
    let store = store().await?;
    let (account, _) = tenant(&store.db, "racing").await?;
    let now = OffsetDateTime::now_utc();
    let later = now + time::Duration::hours(1);
    let idle = std::time::Duration::from_secs(3600);

    assert!(
        store
            .db
            .create_account_session_if_current("current", account, "hash-of-racing", now, later)
            .await?
    );
    assert!(
        store
            .db
            .account_session_at("current", now, idle)
            .await?
            .is_some()
    );

    store
        .db
        .set_account_password(account, "hash-of-racing", "a-newer-hash", "current")
        .await?;
    assert!(
        !store
            .db
            .create_account_session_if_current("stale", account, "hash-of-racing", now, later)
            .await?,
        "a sign-in that verified the old hash got a session after the change"
    );
    assert!(
        store
            .db
            .account_session_at("stale", now, idle)
            .await?
            .is_none()
    );

    store.db.delete_account(account, None, ROOMY).await?;
    assert!(
        !store
            .db
            .create_account_session_if_current("ghost", account, "a-newer-hash", now, later)
            .await?
    );
    Ok(())
}

/// A deletion that verified a password which has since been changed deletes nothing:
/// whoever changed the password did it to shut that request out.
#[tokio::test]
async fn a_deletion_that_proved_an_older_password_is_refused() -> anyhow::Result<()> {
    let store = store().await?;
    let (account, workspace) = tenant(&store.db, "racing-delete").await?;
    fill(&store, account, workspace).await?;
    let before = counts(&store.raw, workspace).await?;
    let old = store
        .db
        .account_password_hash(account)
        .await?
        .expect("the account has a password");
    assert!(
        store
            .db
            .set_account_password(account, &old, "the-new-hash", "none")
            .await?
    );

    assert!(matches!(
        store.db.delete_account(account, Some(&old), ROOMY).await,
        Err(QueryError::Invalid(_))
    ));
    assert_eq!(counts(&store.raw, workspace).await?, before);
    assert!(store.db.account_by_id(account).await?.is_some());

    // With the current hash the same request goes through.
    assert_eq!(
        store
            .db
            .delete_account(account, Some("the-new-hash"), ROOMY)
            .await?,
        vec![workspace]
    );
    Ok(())
}

/// A monitor is never inserted into a workspace that no longer exists. A create waits
/// on a DNS answer before its insert; an account deletion can remove the workspace in
/// that wait, and a monitor left behind would be unreachable and probed for ever.
#[tokio::test]
async fn no_monitor_is_created_in_a_workspace_that_was_deleted() -> anyhow::Result<()> {
    let store = store().await?;
    let (account, workspace) = tenant(&store.db, "gone").await?;
    assert_eq!(
        store.db.delete_account(account, None, ROOMY).await?,
        vec![workspace]
    );

    let late = NewMonitor::new("late", url::Url::parse("https://target.example/health")?)?
        .into_monitor_for(workspace);
    assert!(
        !store
            .db
            .workspace(workspace)
            .insert_monitor_within(&late, 10)
            .await?,
        "a monitor landed in a deleted workspace"
    );
    assert_eq!(
        scalar(
            &store.raw,
            "SELECT COUNT(*) FROM monitors WHERE workspace_id = ?",
            &workspace.0.to_string()
        )
        .await?,
        0
    );
    Ok(())
}

/// If a monitor does outlive its workspace (a row from before that rule), recording
/// its checks must not fail: a failed commit counts against the engine's health for
/// every workspace. Three failures open its incident like anyone's.
#[tokio::test]
async fn a_monitor_without_a_workspace_row_still_commits() -> anyhow::Result<()> {
    let store = store().await?;
    let (_account, workspace) = tenant(&store.db, "left-behind").await?;
    let mut orphan = NewMonitor::new("orphan", url::Url::parse("https://target.example/health")?)?
        .into_monitor_for(workspace);
    store
        .db
        .workspace(workspace)
        .insert_monitor(&orphan)
        .await?;
    // The row as it would be after its workspace went: pointing at nothing.
    let nowhere = WorkspaceId::new();
    sqlx::query("UPDATE monitors SET workspace_id = ? WHERE id = ?")
        .bind(nowhere.0.to_string())
        .bind(orphan.id.0.to_string())
        .execute(&store.raw)
        .await?;
    orphan.workspace = nowhere;
    for _ in 0..3 {
        record(&store, &orphan, CheckOutcome::HttpFail { status: 503 }).await?;
    }
    assert_eq!(store.db.workspace(nowhere).open_incidents().await?.len(), 1);
    Ok(())
}

/// The deletion's bound counts everything its one transaction removes, across every
/// workspace it removes: maintenance windows, API tokens and pending invitations go
/// in cascades and are rows like any other.
#[tokio::test]
async fn the_deletion_bound_counts_what_cascades_too() -> anyhow::Result<()> {
    let store = store().await?;
    let (account, workspace) = tenant(&store.db, "cascades").await?;
    fill(&store, account, workspace).await?;
    let id = workspace.0.to_string();
    let mut rows = 0;
    for sql in [
        "SELECT COUNT(*) FROM checks WHERE workspace_id = ?",
        "SELECT COUNT(*) FROM incidents WHERE workspace_id = ?",
        "SELECT COUNT(*) FROM notify_outbox WHERE workspace_id = ?",
        "SELECT COUNT(*) FROM maintenance_windows WHERE workspace_id = ?",
        "SELECT COUNT(*) FROM api_tokens WHERE workspace_id = ?",
        "SELECT COUNT(*) FROM workspace_invitations WHERE workspace_id = ?",
    ] {
        let held = scalar(&store.raw, sql, &id).await?;
        assert!(held > 0, "the fixture holds a row for: {sql}");
        rows += held;
    }
    assert!(matches!(
        store.db.delete_account(account, None, rows - 1).await,
        Err(QueryError::Conflict(_))
    ));
    assert_eq!(
        store.db.delete_account(account, None, rows).await?,
        vec![workspace]
    );
    Ok(())
}
