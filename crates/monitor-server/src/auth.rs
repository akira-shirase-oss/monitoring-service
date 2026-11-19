//! Management auth (M5.1 + M7.2): every `/api/**` route requires a credential once
//! `SMON_ADMIN_TOKEN` is configured — either `Authorization: Bearer <token>` (CLI,
//! scripts) or the `HttpOnly` `smon_session` cookie the dashboard earns at `/api/login`.
//! Denied means denied — there is no bypass path inside the middleware; the only
//! unauthenticated mode is dev-on-loopback, and boot refuses a tokenless
//! non-loopback listen rather than leave the store open.
//!
//! Comparison is done on SHA-256 digests: fixed-length, content-independent, so the
//! compare can't leak the token's prefix or length through timing.
//!
//! Sessions (D-016): the cookie carries a random id; the `sessions` row is keyed by
//! [`session_key`] — a digest of that id bound to the admin token. The store and its
//! backups hold nothing that works as a cookie, and rotating `SMON_ADMIN_TOKEN` ends every
//! session at once.
//!
//! CSRF contract (M7.2): a request that carries an `Origin` header must send one that
//! matches the `Host` — a cross-site browser request is rejected no matter which
//! credential it rides. A *cookie*-authed mutation must send `Origin` at all:
//! browsers always do on writes, so a missing header is not a browser. Bearer
//! requests need no Origin — `curl` and the CLI don't send one.

use axum::extract::State;
use axum::http::{Method, Request, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use monitor_domain::{AccountId, ApiTokenId, Role, WorkspaceId};

use crate::AppState;

/// The configured credential. `None` is only reachable on a loopback listen — main.rs
/// refuses to boot otherwise.
pub type AdminToken = Option<Arc<str>>;

/// The `HttpOnly` cookie name the session id rides on.
pub const SESSION_COOKIE: &str = "smon_session";

/// When the session cookie carries `Secure` (`SMON_COOKIE_SECURE`). The server itself
/// speaks plain HTTP; TLS is terminated in front of it, so "was this request TLS" is what
/// the proxy reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CookieSecure {
    /// Whenever the request reached the proxy over TLS: `X-Forwarded-Proto: https` or
    /// `Forwarded: proto=https`. Believing a client that sends these itself is safe in this
    /// direction — it can only *add* the flag to its own cookie, never remove it.
    #[default]
    Auto,
    /// On every cookie — for a proxy that does not forward the scheme.
    Always,
}

impl CookieSecure {
    /// The cookie attribute for this request: `"; Secure"` or nothing.
    pub fn attribute(self, headers: &header::HeaderMap) -> &'static str {
        if self == Self::Always || came_over_tls(headers) {
            "; Secure"
        } else {
            ""
        }
    }
}

/// Any `https` in `X-Forwarded-Proto` (a list when proxies chain) or a `proto=https` pair
/// in `Forwarded` (RFC 7239). "Any", because adding `Secure` is the safe direction.
fn came_over_tls(headers: &header::HeaderMap) -> bool {
    let values = |name| {
        headers
            .get_all(name)
            .into_iter()
            .filter_map(|value| value.to_str().ok())
    };
    let forwarded_proto = values("x-forwarded-proto")
        .flat_map(|value| value.split(','))
        .any(|proto| proto.trim().eq_ignore_ascii_case("https"));
    let forwarded = values("forwarded")
        .flat_map(|value| value.split([',', ';']))
        .filter_map(|pair| pair.split_once('='))
        .any(|(key, value)| {
            key.trim().eq_ignore_ascii_case("proto")
                && value.trim().trim_matches('"').eq_ignore_ascii_case("https")
        });
    forwarded_proto || forwarded
}

pub async fn require_auth(
    State(state): State<AppState>,
    mut request: Request<axum::body::Body>,
    next: Next,
) -> Response {
    // An Origin that doesn't match the Host is a cross-site browser request —
    // rejected before the credential is even considered, in every mode. `Host` may be
    // absent on HTTP/2 — the URI's authority is the same information.
    let authority = request
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
        .or_else(|| request.uri().authority().map(|a| a.as_str().to_owned()));
    if let Some(origin) = request.headers().get(header::ORIGIN)
        && !origin_matches_host(origin, authority.as_deref())
    {
        return deny(
            StatusCode::FORBIDDEN,
            "origin_mismatch",
            "Origin does not match Host",
        );
    }

    // A presented cookie is resolved before anything else (S1b): an account session
    // authorizes in every mode — tokenless dev included — and scopes to a workspace
    // the account belongs to, never the operator's.
    let session = match session_principal(&state, request.headers().get(header::COOKIE)).await {
        Ok(session) => session,
        Err(response) => return *response,
    };

    let Some(token) = &state.admin_token else {
        // Dev-on-loopback mode (boot refuses it on any other listen). Whoever reaches the
        // listener is trusted — so only a loopback `Host` is: a page that rebinds its own
        // hostname to 127.0.0.1 is same-origin with us, but its `Host` is its own name.
        // A resolved account cookie still binds its own workspace over the dev trust.
        if let Some(session) = session {
            let principal =
                cookie_principal(&state, request.headers(), request.method(), session).await;
            return match principal {
                Ok(principal) => {
                    request.extensions_mut().insert(CookieAuth);
                    request.extensions_mut().insert(principal);
                    next.run(request).await
                }
                Err(response) => *response,
            };
        }
        if let Some(authority) = authority.as_deref()
            && !is_loopback_authority(authority)
        {
            return deny(
                StatusCode::FORBIDDEN,
                "host_not_allowed",
                "without SMON_ADMIN_TOKEN only a loopback Host is served",
            );
        }
        request.extensions_mut().insert(Principal::Operator {
            workspace: WorkspaceId::DEFAULT,
        });
        return next.run(request).await;
    };

    // A presented bearer draws on its address's guessing budget before it is compared
    // (throttle.rs, L1); a right one gives the token back. An operator bearer wins;
    // anything else is tried as a workspace API token by digest.
    let bearer = match resolve_bearer(&state, request.extensions(), request.headers(), token).await
    {
        Ok(bearer) => bearer,
        Err(response) => return *response,
    };

    match bearer {
        Bearer::Operator => match scope_operator(&state, request.headers()).await {
            Ok(workspace) => {
                request
                    .extensions_mut()
                    .insert(Principal::Operator { workspace });
                next.run(request).await
            }
            Err(response) => *response,
        },
        Bearer::Token(principal) => {
            // A token's scope is the row that issued it — the picker can't move it.
            if let Principal::Token { workspace, .. } = principal
                && matches!(workspace_pick(request.headers()), Ok(Some(picked)) if picked != workspace)
            {
                return deny_not_a_member();
            }
            request.extensions_mut().insert(principal);
            next.run(request).await
        }
        _ => {
            let Some(session) = session else {
                return deny(
                    StatusCode::UNAUTHORIZED,
                    "unauthenticated",
                    "a valid Bearer token or session cookie is required",
                );
            };
            match cookie_principal(&state, request.headers(), request.method(), session).await {
                Ok(principal) => {
                    request.extensions_mut().insert(CookieAuth);
                    request.extensions_mut().insert(principal);
                    next.run(request).await
                }
                Err(response) => *response,
            }
        }
    }
}

/// A session cookie's principal (S1c): the Origin rule for cookie-authenticated writes
/// holds in every mode, then the session scopes — an account to a workspace it's a
/// member of, an operator to the picked workspace.
async fn cookie_principal(
    state: &AppState,
    headers: &header::HeaderMap,
    method: &Method,
    session: Session,
) -> Result<Principal, Box<Response>> {
    if is_mutation(method) && headers.get(header::ORIGIN).is_none() {
        return Err(Box::new(deny(
            StatusCode::FORBIDDEN,
            "origin_required",
            "cookie-authenticated writes require a matching Origin",
        )));
    }
    match session {
        Session::Account(account, home) => scope_account(state, headers, account, home).await,
        Session::Operator => match scope_operator(state, headers).await {
            Ok(workspace) => Ok(Principal::Operator { workspace }),
            Err(response) => Err(response),
        },
    }
}

/// The operator's workspace: the default, or one `X-Smon-Workspace` names — a
/// superuser may act in any workspace that exists, but a misspelt id is still a miss.
async fn scope_operator(
    state: &AppState,
    headers: &header::HeaderMap,
) -> Result<WorkspaceId, Box<Response>> {
    match workspace_pick(headers) {
        Ok(Some(workspace)) => match state.db.workspace_row(workspace).await {
            Ok(Some(_)) => Ok(workspace),
            Ok(None) => Err(Box::new(deny_not_a_member())),
            Err(error) => {
                tracing::error!(%error, "workspace lookup failed");
                Err(Box::new(deny(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "internal",
                    "the workspace store is unavailable",
                )))
            }
        },
        Ok(None) => Ok(WorkspaceId::DEFAULT),
        Err(()) => Err(Box::new(deny_not_a_member())),
    }
}

/// What an `Authorization` header proved, if anything: the operator token, a live
/// workspace API token (already a `Principal`), or a guess that didn't land.
enum Bearer {
    Operator,
    Token(Principal),
    Denied,
    Absent,
}

/// The bearer verdict: a presented credential draws on its address's guessing budget
/// before it's compared (throttle.rs, L1); a right one gives the token back. The
/// operator token wins; anything else is tried as a workspace API token by digest.
async fn resolve_bearer(
    state: &AppState,
    extensions: &axum::http::Extensions,
    headers: &header::HeaderMap,
    token: &Arc<str>,
) -> Result<Bearer, Box<Response>> {
    let Some(presented) = headers.get(header::AUTHORIZATION) else {
        return Ok(Bearer::Absent);
    };
    let Some(guard) = extensions.get::<crate::throttle::Guard>() else {
        tracing::error!("invariant: routes() installs the credential guard");
        return Err(Box::new(deny(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            "internal error",
        )));
    };
    let key = guard.client(extensions, headers);
    let attempt = match guard.throttle.admit(key) {
        Err(wait) => return Err(Box::new(crate::throttle::too_many(wait))),
        Ok(attempt) => attempt,
    };
    let Some(presented) = bearer_str(presented) else {
        return Ok(Bearer::Denied);
    };
    if digests_match(presented.as_bytes(), token.as_bytes()) {
        attempt.succeeded();
        return Ok(Bearer::Operator);
    }
    match api_token_principal(state, presented).await {
        Ok(Some(principal)) => {
            attempt.succeeded();
            Ok(Bearer::Token(principal))
        }
        Ok(None) => Ok(Bearer::Denied),
        Err(response) => Err(response),
    }
}

/// The bearer value itself, without the scheme — `None` when it isn't Bearer or isn't
/// UTF-8 (a non-UTF-8 credential is a denial, not an error).
fn bearer_str(value: &header::HeaderValue) -> Option<&str> {
    value.to_str().ok()?.strip_prefix("Bearer ")
}

/// One deny for every workspace-pick failure — malformed id, workspace that doesn't
/// exist, workspace the caller isn't a member of, workspace a token can't reach. Which
/// it was stays in the server's logs, not the response.
fn deny_not_a_member() -> Response {
    deny(
        StatusCode::FORBIDDEN,
        "not_a_member",
        "you are not a member of that workspace",
    )
}

/// `X-Smon-Workspace` as a workspace id: absent → `Ok(None)`; unparseable → `Err`, a
/// workspace the caller can't name is one they can't be a member of.
fn workspace_pick(headers: &header::HeaderMap) -> Result<Option<WorkspaceId>, ()> {
    let Some(value) = headers.get(WORKSPACE_HEADER) else {
        return Ok(None);
    };
    let parsed = value
        .to_str()
        .ok()
        .and_then(|raw| uuid::Uuid::parse_str(raw).ok());
    match parsed {
        Some(id) => Ok(Some(WorkspaceId(id))),
        None => Err(()),
    }
}

/// The account session's scope (S1c): the picked workspace or the session's own,
/// and the role the `members` row grants there — no row, no request.
async fn scope_account(
    state: &AppState,
    headers: &header::HeaderMap,
    account: AccountId,
    home: WorkspaceId,
) -> Result<Principal, Box<Response>> {
    let workspace = match workspace_pick(headers) {
        Ok(picked) => picked.unwrap_or(home),
        Err(()) => return Err(Box::new(deny_not_a_member())),
    };
    match state.db.member_role(workspace, account).await {
        Ok(Some(role)) => Ok(Principal::Account {
            account,
            workspace,
            role,
        }),
        Ok(None) => Err(Box::new(deny_not_a_member())),
        Err(error) => {
            tracing::error!(%error, "membership lookup failed");
            Err(Box::new(deny(
                StatusCode::SERVICE_UNAVAILABLE,
                "internal",
                "the membership store is unavailable",
            )))
        }
    }
}

/// A workspace API token by digest (S1c): the row is the credential — its workspace,
/// its minted role, its revocation. `live()` is the whole liveness check; a revoked
/// token answers like an unknown one. Last-use is stamped in memory and flushed in
/// batches, never written per request.
async fn api_token_principal(
    state: &AppState,
    presented: &str,
) -> Result<Option<Principal>, Box<Response>> {
    let digest = api_token_key(presented);
    let row = match state.db.api_token_by_digest(&digest).await {
        Ok(row) => row,
        Err(error) => {
            tracing::error!(%error, "api token lookup failed");
            return Err(Box::new(deny(
                StatusCode::SERVICE_UNAVAILABLE,
                "internal",
                "the credential store is unavailable",
            )));
        }
    };
    let Some(row) = row else { return Ok(None) };
    if !row.live() || !digests_match(digest.as_bytes(), row.token_digest.as_bytes()) {
        return Ok(None);
    }
    let (Some(token), Some(workspace), Some(role)) = (row.token(), row.workspace(), row.role())
    else {
        tracing::error!(id = %row.id, "unreadable api token row");
        return Err(Box::new(deny(
            StatusCode::SERVICE_UNAVAILABLE,
            "internal",
            "the credential store is unavailable",
        )));
    };
    {
        let mut pending = state
            .token_use
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if pending.len() < TOKEN_USE_CAP || pending.contains_key(&token) {
            pending.insert(token, (state.clock)());
        }
    }
    Ok(Some(Principal::Token {
        token,
        workspace,
        role,
    }))
}

/// Resolve a presented session cookie into its principal (S1b): operator sessions are
/// tried first when a token exists, then account sessions — the two key namespaces
/// can't collide. `Err` is the honest "the store can't answer right now", never a 401
/// that would masquerade a broken store as a wrong credential.
/// What a presented session cookie proved: an operator session, or an account session
/// carrying the account and its home workspace — `require_auth` turns it into a
/// scoped `Principal` after the workspace picker and the members row have their say.
enum Session {
    Operator,
    Account(AccountId, WorkspaceId),
}

async fn session_principal(
    state: &AppState,
    cookie: Option<&header::HeaderValue>,
) -> Result<Option<Session>, Box<Response>> {
    let Some(session) = session_cookie(cookie) else {
        return Ok(None);
    };
    if let Some(token) = &state.admin_token {
        match state
            .db
            .session_valid_at(
                &session_key(token, session),
                (state.clock)(),
                state.session_idle,
            )
            .await
        {
            Ok(true) => return Ok(Some(Session::Operator)),
            Ok(false) => {}
            Err(error) => {
                tracing::error!(%error, "session lookup failed");
                return Err(Box::new(deny(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "internal",
                    "the session store is unavailable",
                )));
            }
        }
    }
    match state
        .db
        .account_session_at(
            &account_session_key(session),
            (state.clock)(),
            state.session_idle,
        )
        .await
    {
        Ok(Some((account, home))) => Ok(Some(Session::Account(account, home))),
        Ok(None) => Ok(None),
        Err(error) => {
            tracing::error!(%error, "account session lookup failed");
            Err(Box::new(deny(
                StatusCode::SERVICE_UNAVAILABLE,
                "internal",
                "the session store is unavailable",
            )))
        }
    }
}

/// Request marker: this call was authorized by the session cookie, not a bearer token.
/// Handlers that care (none yet — parity is the point) can read it from extensions.
#[derive(Debug, Clone, Copy)]
pub struct CookieAuth;

/// `Origin: http://host[:port]` must name the same authority the request was sent to —
/// host plus port, not scheme (a TLS-terminating proxy still terminates the same site).
pub fn origin_matches_host(origin: &header::HeaderValue, authority: Option<&str>) -> bool {
    let (Ok(origin), Some(authority)) = (origin.to_str(), authority) else {
        return false;
    };
    let Ok(parsed) = url::Url::parse(origin) else {
        return false;
    };
    matches!(parsed.scheme(), "http" | "https")
        && parsed
            .host_str()
            .is_some_and(|h| authority_matches(h, parsed.port(), authority))
}

/// `localhost`, `127.0.0.0/8` or `[::1]`, with any port. A request with no Host at all is
/// not a browser (browsers always send one) and is decided by the caller.
fn is_loopback_authority(authority: &str) -> bool {
    match url::Url::parse(&format!("http://{authority}/")) {
        Ok(url) => match url.host() {
            Some(url::Host::Domain(name)) => {
                name.trim_end_matches('.').eq_ignore_ascii_case("localhost")
            }
            Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
            Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
            None => false,
        },
        Err(_) => false,
    }
}

/// `authority` is `host[:port]` from the URL; `host_header` is the raw Host value
/// (same shape). Compare case-insensitively on host and exactly on the effective port —
/// default ports are equal by URL rules (`http://x` == `x:80` is NOT assumed here: the
/// browser writes the explicit port it dialed, so string authority compare is right).
fn authority_matches(host: &str, port: Option<u16>, host_header: &str) -> bool {
    let normalized = match port {
        Some(port) => format!("{host}:{port}"),
        None => host.to_string(),
    };
    normalized.eq_ignore_ascii_case(host_header)
}

fn is_mutation(method: &Method) -> bool {
    !matches!(
        *method,
        Method::GET | Method::HEAD | Method::OPTIONS | Method::TRACE
    )
}

/// The session id from the Cookie header — name-prefix match, value must be non-empty.
/// `pub` so `logout` parses the cookie through the same code the middleware trusts.
pub fn session_cookie(cookie: Option<&header::HeaderValue>) -> Option<&str> {
    cookie?.to_str().ok()?.split(';').find_map(|pair| {
        let pair = pair.trim();
        let value = pair.strip_prefix(SESSION_COOKIE)?.strip_prefix('=')?;
        (!value.is_empty()).then_some(value)
    })
}

fn deny(status: StatusCode, code: &'static str, message: &'static str) -> Response {
    (
        status,
        axum::Json(serde_json::json!({
            "error": { "code": code, "message": message }
        })),
    )
        .into_response()
}

/// The `sessions` row key for a cookie id: hex `SHA-256("smon-session-v1\0" ‖
/// SHA-256(admin token) ‖ id)`. One-way, so a copied store can't be replayed as cookies;
/// bound to the token, so a cookie earned under an old token maps to a key no row has.
pub fn session_key(admin_token: &str, cookie_id: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(b"smon-session-v1\0");
    digest.update(Sha256::digest(admin_token.as_bytes()));
    digest.update(cookie_id.as_bytes());
    crate::deliver::hex_lower(&digest.finalize())
}

/// Hash-then-compare: both sides are 32-byte digests, so the equality check runs in
/// data-independent time and reveals nothing about *where* the inputs differ.
pub fn digests_match(presented: &[u8], configured: &[u8]) -> bool {
    let presented = Sha256::digest(presented);
    let configured = Sha256::digest(configured);
    presented == configured
}

/// The `invites` row key for a presented code (S1b): hex `SHA-256("smon-invite-v1\0" ‖
/// code)`. The store holds only digests; comparison runs on fixed-length hashes.
pub fn invite_key(code: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(b"smon-invite-v1\0");
    digest.update(code.as_bytes());
    crate::deliver::hex_lower(&digest.finalize())
}

/// The `sessions` row key for an account cookie: hex `SHA-256("smon-acct-session-v1\0" ‖
/// id)`. Same digest-only rule as `session_key` (D-016) — the row can never be replayed
/// as a cookie — namespaced apart so an account id collides with nothing an operator
/// session could produce.
pub fn account_session_key(cookie_id: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(b"smon-acct-session-v1\0");
    digest.update(cookie_id.as_bytes());
    crate::deliver::hex_lower(&digest.finalize())
}

/// Who authorized a request (S1b/S1c): the operator (bearer token, operator session,
/// or loopback dev trust) acts at owner level on the default workspace — or on one
/// named by `X-Smon-Workspace`; an account session acts at its member role in the
/// workspace it picked; a workspace API token acts at the role minted into it, in the
/// workspace row that issued it and nowhere else. The middleware inserts this into
/// request extensions; `viewer_for`/`member_for`/`owner_for` resolve the store view.
#[derive(Debug, Clone, Copy)]
pub enum Principal {
    Operator {
        workspace: WorkspaceId,
    },
    Account {
        account: AccountId,
        workspace: WorkspaceId,
        role: Role,
    },
    Token {
        token: ApiTokenId,
        workspace: WorkspaceId,
        role: Role,
    },
}

impl Principal {
    /// The workspace this principal may see.
    pub fn workspace(&self) -> WorkspaceId {
        match self {
            Self::Operator { workspace }
            | Self::Account { workspace, .. }
            | Self::Token { workspace, .. } => *workspace,
        }
    }
}

/// `X-Smon-Workspace`: an account may act in any workspace it belongs to, not only the
/// one it signed up with. The header names the workspace by id; membership — and the
/// role the request gets — comes from the `members` row, never the header.
pub const WORKSPACE_HEADER: &str = "X-Smon-Workspace";

/// Batched last-used bookkeeping for workspace API tokens (S1c): the auth path stamps
/// `token → now` in memory; `crate::flush_token_uses` lands the batch in one write —
/// a write per request would put SQLite on the hot path of every API call. Bounded:
/// past `TOKEN_USE_CAP` pending stamps, tracking is skipped rather than grown.
pub type TokenUse =
    std::sync::Arc<std::sync::Mutex<std::collections::HashMap<ApiTokenId, time::OffsetDateTime>>>;

/// The pending-map bound — same headroom as the credential throttle's table.
pub const TOKEN_USE_CAP: usize = 4096;

/// The API-token lookup key: `SHA-256("smon-api-token-v1\0" ‖ token)` — same
/// namespaced-digest rule as invites and sessions; the store never sees the token.
pub fn api_token_key(token: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(b"smon-api-token-v1\0");
    digest.update(token.as_bytes());
    crate::deliver::hex_lower(&digest.finalize())
}

/// Password work is deliberately expensive — argon2id, RFC 9106's low-memory profile
/// (19 MiB, t=2, p=1) — so it runs on the blocking pool behind a hard bound: at most
/// `PASSWORD_WORK_PERMITS` hashes/verifications in flight for the whole process, no
/// matter how many clients arrive at once (S1b). A flood queues; it can't burn the CPU.
static PASSWORD_WORK: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);

/// The queue in front of the permits is bounded too: a request that can't start inside
/// this wait is answered 503 `busy` instead of parking on its connection permit
/// forever — a sign-in flood must not starve the rest of the API (S1c fix).
pub const PASSWORD_WORK_WAIT: Duration = Duration::from_secs(2);

/// Busy answers given so far. The `warn!` fires on the first and every 64th — the
/// counter keeps the log honest under a flood without a line per request.
static PASSWORD_WORK_BUSY: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Why password work couldn't run. `Busy` → the handler answers 503 `busy` with
/// Retry-After; `Hash` is a hashing failure — a tooling bug, not the client's.
#[derive(Debug)]
pub enum PasswordWorkError {
    Busy,
    Hash(argon2::password_hash::Error),
}

/// Wait at most `wait` for a hashing slot; a full queue past the deadline is `Busy`.
async fn password_work_permit(
    wait: Duration,
) -> Result<tokio::sync::SemaphorePermit<'static>, PasswordWorkError> {
    match tokio::time::timeout(wait, PASSWORD_WORK.acquire()).await {
        Ok(Ok(permit)) => Ok(permit),
        Ok(Err(_)) => unreachable!("the password-work semaphore is never closed"),
        Err(_elapsed) => {
            let total = PASSWORD_WORK_BUSY.fetch_add(1, Ordering::Relaxed) + 1;
            if total == 1 || total.is_multiple_of(64) {
                tracing::warn!(
                    busy_total = total,
                    "password work queue saturated; answering busy"
                );
            }
            Err(PasswordWorkError::Busy)
        }
    }
}

/// A fixed argon2id PHC string for the unknown-email path, so a sign-in for an address
/// that has no account pays the same work as a wrong password (no existence oracle).
pub const DUMMY_LOGIN_HASH: &str = "$argon2id$v=19$m=19456,t=2,p=1$c2VvbGEwMDAwMDAwMDAwMA$\
    0J7T0gVh8Z6J8n9m0bYb7Uo5m7G8X0F8J8yK8m0m0m0";

fn argon2() -> argon2::Argon2<'static> {
    argon2::Argon2::default() // argon2id, m=19 MiB / t=2 / p=1 — fixed, bounded
}

/// The argon2id PHC string for a new password, computed under the work limit and
/// inside `wait`. A panicked blocking task is re-thrown — hashing must never
/// silently "fail".
pub async fn hash_password(password: &str, wait: Duration) -> Result<String, PasswordWorkError> {
    let _permit = password_work_permit(wait).await?;
    let owned = password.to_owned();
    match tokio::task::spawn_blocking(move || {
        use argon2::password_hash::PasswordHasher;
        argon2()
            .hash_password(owned.as_bytes())
            .map(|h| h.to_string())
    })
    .await
    {
        Ok(result) => result.map_err(PasswordWorkError::Hash),
        Err(join) => std::panic::resume_unwind(join.into_panic()),
    }
}

/// Constant-work verify: `hash` comes from the store (or `DUMMY_HASH`), the password
/// from the request, `wait` bounds the queue. Malformed stored hashes fail closed
/// (`Ok(false)`), never panic.
pub async fn verify_password(
    hash: &str,
    password: &str,
    wait: Duration,
) -> Result<bool, PasswordWorkError> {
    let _permit = password_work_permit(wait).await?;
    let (hash, owned) = (hash.to_owned(), password.to_owned());
    Ok(tokio::task::spawn_blocking(move || {
        use argon2::password_hash::PasswordVerifier;
        let Ok(parsed) = argon2::PasswordHash::new(&hash) else {
            return false;
        };
        argon2().verify_password(owned.as_bytes(), &parsed).is_ok()
    })
    .await
    .unwrap_or(false))
}

/// Test hook: the work limiter, so a test can prove the bound is real.
#[cfg(test)]
pub fn password_work_permits() -> usize {
    PASSWORD_WORK.available_permits()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn digest_compare_accepts_the_same_bytes() {
        assert!(digests_match(b"s3cret", b"s3cret"));
    }

    #[test]
    fn digest_compare_rejects_any_difference() {
        assert!(!digests_match(b"s3cret", b"s3creT"));
        assert!(!digests_match(b"s3cret", b"s3cret-extra"));
        assert!(!digests_match(b"", b"s3cret"));
    }

    fn origin(value: &str) -> HeaderValue {
        HeaderValue::from_str(value).unwrap()
    }

    #[test]
    fn a_matching_origin_is_same_authority() {
        assert!(origin_matches_host(
            &origin("http://127.0.0.1:58400"),
            Some("127.0.0.1:58400")
        ));
        assert!(origin_matches_host(
            &origin("https://mon.example.com"),
            Some("mon.example.com")
        ));
        // An IPv6 authority keeps its brackets in both places.
        assert!(origin_matches_host(
            &origin("http://[::1]:9000"),
            Some("[::1]:9000")
        ));
    }

    #[test]
    fn a_cross_site_origin_is_rejected() {
        let host = "127.0.0.1:58400";
        for bad in [
            "http://127.0.0.2:58400",
            "http://127.0.0.1:58401",
            "https://evil.example.com",
            "javascript:alert(1)",
            "null",
            "not a url",
        ] {
            assert!(
                !origin_matches_host(&origin(bad), Some(host)),
                "{bad} must not match"
            );
        }
        assert!(!origin_matches_host(&origin("http://x"), None));
        assert!(!origin_matches_host(
            &origin("https://example.com"),
            Some("example.com:443")
        ));
        // The browser spells default ports explicitly; a port-ful Host can't claim a
        // port-less Origin.
        assert!(!origin_matches_host(
            &origin("https://example.com"),
            Some("example.com:8443")
        ));
    }

    #[test]
    fn session_cookie_parses_its_own_name_only() {
        let header = HeaderValue::from_static("other=1; smon_session=abc-123 ; theme=dark");
        assert_eq!(session_cookie(Some(&header)), Some("abc-123"));
        assert_eq!(
            session_cookie(Some(&HeaderValue::from_static("smon_session="))),
            None
        );
        assert_eq!(
            session_cookie(Some(&HeaderValue::from_static("smon_sessionx=abc"))),
            None
        );
        assert_eq!(session_cookie(None), None);
    }

    /// Known answer (computed independently with Python's hashlib): a sha2 upgrade that
    /// changed this would sign every dashboard user out.
    #[test]
    fn a_session_key_has_a_fixed_known_value() {
        assert_eq!(
            session_key("token-a", "cookie-id"),
            "a5bc5ec481ea1ded6473443e073ea2fd423d4829b600bf00bcb1175f3737835c"
        );
    }

    #[test]
    fn a_session_key_is_one_way_and_bound_to_the_token() {
        let key = session_key("token-a", "cookie-id");
        assert_eq!(key.len(), 64, "hex SHA-256");
        assert!(!key.contains("cookie-id"));
        assert_eq!(key, session_key("token-a", "cookie-id"), "deterministic");
        assert_ne!(
            key,
            session_key("token-b", "cookie-id"),
            "bound to the token"
        );
        assert_ne!(key, session_key("token-a", "cookie-ie"), "bound to the id");
    }

    #[test]
    fn only_loopback_authorities_are_loopback() {
        for ok in [
            "127.0.0.1:58465",
            "127.1.2.3",
            "localhost:1",
            "LOCALHOST.:2",
            "[::1]:3",
        ] {
            assert!(is_loopback_authority(ok), "{ok}");
        }
        for bad in [
            "rebound.attacker.example",
            "localhost.attacker.example",
            "127.0.0.1.nip.io",
            "10.0.0.1:58465",
            "[::2]:1",
            "",
            "a b",
        ] {
            assert!(!is_loopback_authority(bad), "{bad}");
        }
    }

    #[test]
    fn mutations_are_everything_but_safe_reads() {
        assert!(is_mutation(&Method::POST));
        assert!(is_mutation(&Method::PATCH));
        assert!(is_mutation(&Method::DELETE));
        assert!(!is_mutation(&Method::GET));
        assert!(!is_mutation(&Method::HEAD));
    }
}

#[cfg(test)]
mod password_tests {
    use super::*;

    /// The dummy hash must parse as argon2id PHC or the unknown-email path would skip
    /// the work — a silent existence oracle. Pin its validity.
    #[test]
    fn the_dummy_hash_is_a_real_argon2id_phc() {
        argon2::PasswordHash::new(DUMMY_LOGIN_HASH)
            .expect("DUMMY_LOGIN_HASH must parse as a PHC string");
    }

    /// Both work permits, however long a sibling test's real hashing keeps them — the
    /// semaphore is process-global and unit tests run in parallel, so a plain
    /// `try_acquire` can find it spent. `try_acquire_many` is atomic — a retry never
    /// strands one permit. Bounded: a real leak still fails.
    async fn hold_both_permits() -> tokio::sync::SemaphorePermit<'static> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            if let Ok(permits) = PASSWORD_WORK.try_acquire_many(2) {
                return permits;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the password-work permits never freed"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    /// The limiter is a hard bound, not a queue hint: two permits, never more. A sibling
    /// test's real hashing can hold one, so the free count is `<= 2`, not `== 2`.
    #[tokio::test]
    async fn password_work_is_bounded() {
        assert!(password_work_permits() <= 2, "the bound is two permits");
        let permits = hold_both_permits().await;
        assert!(
            PASSWORD_WORK.try_acquire().is_err(),
            "a third burns nothing"
        );
        drop(permits);
    }

    #[tokio::test]
    async fn a_hash_verifies_and_a_wrong_password_does_not() {
        let hash = hash_password("s3cret long password", PASSWORD_WORK_WAIT)
            .await
            .expect("hash");
        assert!(
            verify_password(&hash, "s3cret long password", PASSWORD_WORK_WAIT)
                .await
                .expect("verify")
        );
        assert!(
            !verify_password(&hash, "wrong", PASSWORD_WORK_WAIT)
                .await
                .expect("verify")
        );
        assert!(
            !verify_password("not-a-phc", "x", PASSWORD_WORK_WAIT)
                .await
                .expect("verify"),
            "bad store → closed"
        );
    }

    /// A full queue must answer `Busy` inside the deadline — a waiter that parked
    /// forever on its connection permit is how a sign-in flood starves the API.
    #[tokio::test]
    async fn a_full_work_queue_answers_busy_within_the_deadline() {
        let _permits = hold_both_permits().await;
        match verify_password(DUMMY_LOGIN_HASH, "x", PASSWORD_WORK_WAIT).await {
            Err(PasswordWorkError::Busy) => {}
            other => panic!("a full queue is Busy, not {other:?}"),
        }
        match hash_password("x", PASSWORD_WORK_WAIT).await {
            Err(PasswordWorkError::Busy) => {}
            other => panic!("a full queue is Busy, not {other:?}"),
        }
    }
}
