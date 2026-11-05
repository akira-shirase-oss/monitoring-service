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
  `GET /api/monitors/{id}/stats` (coverage/success, `?window_s=` bounded), `GET /api/incidents`
  (`?monitor_id=`, `?state=`, `?limit=`). Still to come: `GET /api/status` (M3.2 public
  snapshot). Incidents are system-created evidence — there is deliberately no `POST`.
- **Error envelope:** `{"error": {"code": stable, "message": safe}}`; internals log-only.
- **Bounds:** 16 KiB bodies, 10 s request deadline, request id set+propagated, panic→500.
- **Auth (M5):** management routes require a credential; read-only status endpoints are
  separately scoped. Denied means denied — there is no "convenient" unauthenticated mode once
  enabled.
- **Writes bump generation** on any schedule/target/paused change (module 01 invariant).
- **CLI parity:** `monitor ping`, `monitors list|add|edit|pause|delete`, `checks`,
  `incidents`, `status` — everything the API can do, nothing more.
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
