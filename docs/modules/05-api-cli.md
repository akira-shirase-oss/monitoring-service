# Module 05 — Management API + CLI (`monitor-server`, `monitor`)

**Status:** management surface complete (M2–M3): `PATCH`/`DELETE /api/monitors/{id}`
(generation bump + optional optimistic `generation` guard → `conflict`), `GET …/checks`,
`GET …/stats` (coverage vs success, `?window_s=` bounded to [60 s, 7 d]), `GET /incidents`
with `monitor_id`/`state` filters; CLI `show/edit/pause/unpause/delete`. Auth remains M5.

## Purpose

One HTTP API for every client — CLI, dashboard, future agents. No private path into the store.

## Contract

- **Routes:** `/livez`, `/readyz`, `/version`; `GET|POST /api/monitors`,
  `GET|PATCH|DELETE /api/monitors/{id}`; `GET /api/monitors/{id}/checks`,
  `GET /api/monitors/{id}/stats` (coverage/success, `?window_s=` bounded), `GET /api/incidents`
  (`?monitor_id=`, `?state=`, `?limit=`); `GET|POST /api/monitors/{id}/maintenance`,
  `DELETE /api/maintenance/{id}` (M6.11 — server-computed `active` flag,
  `starts_at < ends_at` enforced, past windows kept as records); `POST /api/backup`
  (D-019 — takes `{}`, writes a server-named snapshot inside `SMON_BACKUP_DIR`, answers
  `{"name","bytes"}`; a client-supplied path is refused 422; unset dir → 404
  `backup_disabled`). Incidents are system-created evidence — there is deliberately no
  `POST`. `IncidentView` carries all four times (M6.3): `first_fail_at`, `opened_at`,
  `last_fail_at`, `resolved_at` — RFC3339, nullable for pre-M6.3 rows.
  Workspace management (S1–S2, D-020/D-021): `GET|PUT|DELETE /api/workspace/webhook`
  (the tenant channel, SSRF-validated at save); `POST /api/workspace/webhook/test`
  (one real send through the same gate — no outbox row, classified result, one per
  15 s per workspace); `PATCH /api/workspace` (rename, 1–80 visible chars);
  `GET /api/workspaces` (the caller's memberships — account rows + account identity,
  a token's one workspace, the operator's inventory); `GET|PUT
  /api/workspace/status-page` (`{slug, title, monitor_ids}` — `title` empty/null is
  the page's off switch, the monitor set is workspace-scoped, a taken slug is 409).
  Status-page bounds (S2 hardening): ≤ 64 `monitor_ids` (duplicates collapse to
  their first occurrence — never a PK error), title ≤ 120 visible chars, and a
  per-workspace write budget of 6 per rolling 60 s — past it, 429 + `Retry-After`;
  validation refusals don't spend. The public read is outside `/api`:
  `GET /s/{slug}/status.json` (module 07).
- **Per-workspace probe share (S3):** a monitor create/edit is also refused
  `capacity_exceeded` (409) when the workspace's *own* unpaused schedule would need
  more than `MAX_WORKSPACE_SLOTS` (= `MAX_GLOBAL_SLOTS` / 4 = 128) probes in
  flight — one tenant's schedule can't silently crowd the engine; pausing frees
  the share.
- **Plan tiers (S3, D-022):** `workspaces.plan` is `free` (signup's default) or
  `operator`; `PATCH /api/workspaces/{id}/plan` moves a workspace — operator-only
  (a tenant gets 403 trying to lift its own limits). Free is provisionally
  10 monitors and a 60 s interval floor: a create below the floor, or an edit
  proposing one, is refused 409 `plan_limit`; the eleventh monitor is 409
  `limit_reached` on the plan cap. `operator` defers to the server's ceilings —
  the 15 s domain floor, `SMON_MAX_MONITORS` — and to the configured
  `SMON_RETENTION_*` horizons, while `free` sheds evidence on shorter plan-fixed
  ones (2 d checks / 90 d aggregates+incidents / 7 d delivered outbox). Floors
  gate writes only: downgrading a workspace never pauses or deletes what runs.
  `GET /api/workspaces` rows carry `plan`.
- **Error envelope:** `{"error": {"code": stable, "message": safe}}`; internals log-only.
- **Bounds:** 16 KiB bodies, 10 s request deadline, request id set+propagated, panic→500.
- **Auth (M5.1 + M7.2, live):** every `/api/**` route requires a credential —
  `Authorization: Bearer $SMON_ADMIN_TOKEN` (sha256-digest compare) for CLI/scripts,
  or the HttpOnly `__Host-smon_session` cookie for the dashboard. `/livez`, `/readyz` and
  static files stay open. Token unset = dev mode on loopback only; a tokenless
  public bind refuses to boot. Denied means denied — no unauthenticated path once
  enabled.
- **Session login (M7.2):** `POST /api/login {token}` exchanges the admin token for a
  `sessions` row (migration 0007) + `Set-Cookie: __Host-smon_session=<id>; HttpOnly;
  SameSite=Strict; Path=/; Max-Age=<SMON_SESSION_TTL_S, default 43200>; Secure` —
  the `__Host-` pin means `Secure` is unconditional (browsers reject the name without
  it) — there is no `off`: sign-in is HTTPS-only in every profile, so a self-hosted
  `http://` install must put TLS in front before the dashboard can be used. Logout's
  expiring cookie carries the
  same shape. Store-side
  `expires_at` is the authority — the cookie's Max-Age is a browser hint. The token
  itself never lands in browser storage; it can be rotated without stranding copies.
  `POST /api/logout` deletes the row and expires the cookie — idempotent. A wrong
  token gets 401 after a ~300 ms courtesy delay (at most 16 sleeping at once). Every
  presented credential — a login or a bearer — first takes a token from its client
  address's bucket (`throttle.rs`: IPv4, or IPv6 by /64; 5 back to back, one more every
  2 s; the right credential gives it back), so a throttled address gets 429 +
  `Retry-After` before the token is compared and one client's flood never locks another
  out. Behind a proxy, `SMON_TRUSTED_PROXY` names it so the right-most
  `X-Forwarded-For` entry is the client. The row is keyed by a token-bound digest of the cookie's id, never
  the id (D-016): rotating the token ends every session, and a session unused for
  `SMON_SESSION_IDLE_S` (default 3600) ends too. A session-store
  failure answers 503, never a false 401. With no admin token configured, login
  answers 404 `auth_disabled` — dev mode has no credential to exchange.
- **CSRF contract:** an `Origin` header that doesn't match `Host` (host:port —
  browsers state the exact authority they dialed) is rejected for *any* credential;
  a **cookie-authed mutation must send Origin** (browsers always do on writes —
  its absence means the request isn't a browser). Bearer requests need no Origin —
  curl and the CLI don't send one.
- **Writes bump generation** on any schedule/target/threshold/paused change (module 01
  invariant). `failures_to_open` / `passes_to_recover` (`1..=10`, default 3/2) are settable
  on create and patch; out-of-range → `invalid_request`.
- **Probe contract (M8):** `method` is `GET` (default) or `HEAD`, and `expect`
  carries `statuses` — ≤8 non-overlapping inclusive ranges (`"200-299,418"`,
  each endpoint 1–599, `lo ≤ hi`) — and `body_contains`, a literal 1–256-byte
  marker searched in the first 64 KiB received inside the probe deadline
  (never a regex; the client never decompresses, so compressed wire bytes are
  searched as-is). `HEAD` + `body_contains` is refused at write — there is no
  body to read — and a marker miss on an acceptable status commits
  `http_fail` + `detail: "assert_body"` (D-025). Create and PATCH apply the
  same rules — an edit proposing bad ranges or a HEAD/marker pair is 422 —
  and the patch can shrink but never skip validation. None of it reaches the
  public `status.json` (module 07's allowlist).
- **CLI parity:** `monitor ping`, `monitors list|add|show|edit|pause|unpause|delete`,
  `maintenance add|list|rm`, `backup` — every management operation the API offers.
  `monitor maintenance add <id> --from now|<rfc3339> --to <rfc3339> [--reason s]` — a bad
  timestamp is a usage error (exit 2), not a round-tripped 422. System endpoints
  (`/livez`, `/readyz`, `/version`) are read through `ping`, not wrapped as commands.
- **CLI exit codes (M6.9, pinned by `tests/exit_codes.rs`):** `0` success; `1` an API or
  application error (envelope errors like `not_found`/`conflict`/`invalid_request`, or an
  unexpected response); `2` usage error (clap rejects, empty `edit`); `3` server
  unreachable (connect refused, DNS, timeout — no HTTP answer at all); `4` unauthorized
  (401/403 — the token is missing or wrong); `5` (`ping` only) degraded — `/readyz`
  answered 200 `"status":"degraded"`: observing, but a supporting duty stopped; the faults
  print to stderr. Errors print to stderr, results to stdout.
- **Truthful readiness:** `/readyz` fails when the store doesn't answer (module 02 failure
  case: disk write fails → unready, not green).

## Failure cases

| Trigger | Required result |
|---|---|
| Store down | `/readyz` 503; `/api/*` error envelope; dashboard shows "cannot see" |
| Oversized body | rejected before parse |
| Invalid input | `invalid_request` with a safe message |
| Stale-generation write | conflict, never silent overwrite |

## Testing parameters

- **Integration (in-memory SQLite):** every route's success + envelope paths; generation
  conflict on PATCH; pause invalidates late results.
- **CLI black-box:** run the real binary against a real server (assert output + exit codes).
- **Auth (M5):** missing/wrong/right credential matrix.
- **Load:** management API at 50 rps while probes run — no probe slot starvation (soak
  assertion); body-limit and timeout behavior under k6.
