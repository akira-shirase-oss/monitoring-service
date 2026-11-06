# Module 05 — Management API + CLI (`monitor-server`, `monitor`)

**Status:** management surface complete (M2–M3): `PATCH`/`DELETE /api/monitors/{id}`
(generation bump + optional optimistic `generation` guard → `conflict`), `GET …/checks`,
`GET …/stats` (coverage vs success, `?window_s=` bounded to [60 s, 7 d]), `GET /incidents`
with `monitor_id`/`state` filters; CLI `show/edit/pause/unpause/delete`. Auth remains M5.

## Purpose

One HTTP API for every client — CLI, dashboard, future agents. No private path into the store.

## Contract

- **Routes:** `/livez`, `/readyz`; `GET|POST /api/monitors`,
  `GET|PATCH|DELETE /api/monitors/{id}`; `GET /api/monitors/{id}/checks`,
  `GET /api/monitors/{id}/stats` (coverage/success, `?window_s=` bounded), `GET /api/incidents`;
  `GET|POST /api/monitors/{id}/maintenance`, `DELETE /api/maintenance/{id}` (M6.11 —
  server-computed `active` flag, `starts_at < ends_at` enforced, past windows kept as
  records). CLI: `monitor maintenance add <id> --from now|<rfc3339> --to <rfc3339>
  [--reason s]`, `maintenance list <id>`, `maintenance rm <window-id>` — a bad timestamp
  is a usage error (exit 2), not a round-tripped 422.
  (`?monitor_id=`, `?state=`, `?limit=`). Still to come: `GET /api/status` (M3.2 public
  snapshot). Incidents are system-created evidence — there is deliberately no `POST`.
  `IncidentView` carries all four times (M6.3): `first_fail_at`, `opened_at`,
  `last_fail_at`, `resolved_at` — RFC3339, nullable for pre-M6.3 rows.
- **Error envelope:** `{"error": {"code": stable, "message": safe}}`; internals log-only.
- **Bounds:** 16 KiB bodies, 10 s request deadline, request id set+propagated, panic→500.
- **Auth (M5.1, live):** every `/api/**` route requires `Authorization: Bearer
  $SMON_ADMIN_TOKEN` (sha256-digest compare). `/livez`, `/readyz` and static files stay
  open. Token unset = dev mode on loopback only; a tokenless public bind refuses to boot.
  Denied means denied — no unauthenticated path once enabled.
- **Writes bump generation** on any schedule/target/threshold/paused change (module 01
  invariant). `failures_to_open` / `passes_to_recover` (`1..=10`, default 3/2) are settable
  on create and patch; out-of-range → `invalid_request`.
- **CLI parity:** `monitor ping`, `monitors list|add|edit|pause|delete`, `checks`,
  `incidents`, `status` — everything the API can do, nothing more.
- **CLI exit codes (M6.9, pinned by `tests/exit_codes.rs`):** `0` success; `1` an API or
  application error (envelope errors like `not_found`/`conflict`/`invalid_request`, or an
  unexpected response); `2` usage error (clap rejects, empty `edit`); `3` server
  unreachable (connect refused, DNS, timeout — no HTTP answer at all); `4` unauthorized
  (401/403 — the token is missing or wrong). Errors print to stderr, results to stdout.
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
