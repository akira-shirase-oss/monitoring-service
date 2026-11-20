# UI audit — every operation the API/CLI offers vs the dashboard

The contract rule: one HTTP API for every client; the dashboard is an operator UI over it.
This table is the coverage check — every row is either an affordance in the UI or an
explicit, reasoned exclusion. Re-run when either side adds a route or command
(`tests/auth_inventory.rs` pins the route list; `docs/modules/05-api-cli.md` pins the CLI).

## API routes

| Route | Dashboard affordance | Notes |
|---|---|---|
| `GET /livez` | — | system probe; `ping` in the CLI. Not an operator action — deliberate exclusion |
| `GET /readyz` | header banners (unreachable / monitoring unavailable / degraded) | readiness is *shown*, never hidden behind a green board |
| `GET /version` | — | upgrade/rollback check; deliberate exclusion (operator CLI/log) |
| `POST /api/login` | sign-in gate | earns the `__Host-smon_session` cookie |
| `POST /api/logout` | sign-out button | deletes the session row + expires the cookie |
| `GET /api/monitors` | monitors list | name, url, interval, status badge |
| `POST /api/monitors` | "Add monitor" form | name, url, interval, timeout, thresholds |
| `GET /api/monitors/{id}` | detail page | badges, cadence line, generation |
| `PATCH /api/monitors/{id}` | pause/resume button; edit form (name, url, interval, timeout, thresholds) | sends the optimistic `generation` guard; 409 shows the server message |
| `DELETE /api/monitors/{id}` | "Delete" → "Confirm delete" | 409 on a monitor with history shows the server's conflict message |
| `GET /api/monitors/{id}/checks` | "Recent checks" strip + rows | includes `missed` — evidence, not interpolation |
| `GET /api/monitors/{id}/stats` | coverage/success line | `no data` when the denominator is undefined |
| `GET /api/monitors/{id}/maintenance` | "Maintenance windows" list | server-computed `active` badge |
| `POST /api/monitors/{id}/maintenance` | "Add window" form (from/until/reason; empty from = next slot) | added this slice |
| `DELETE /api/maintenance/{id}` | per-window "Cancel" → "Confirm cancel" | added this slice |
| `GET /api/incidents` | detail page "Incidents" (per-monitor) | no global incidents page — the list exists per monitor; a global view is a deliberate later call, not parity debt |
| `POST /api/backup` | "Back up now" → "Confirm backup" | added this slice; echoes `name`+`bytes` the server reports |
| `GET /status.json` | `/status` in-app page | plus the publicly-hosted copy off-box |

## CLI commands

| Command | Dashboard affordance | Notes |
|---|---|---|
| `monitor ping` | header banners (same readiness truth `/readyz` reports) | no literal button — the banner IS the ping, shown continuously |
| `monitors list` | monitors list | |
| `monitors add` | "Add monitor" | |
| `monitors show` | detail page | checks + incidents inline |
| `monitors edit` | edit form (all fields incl. name/url) | |
| `monitors pause` / `unpause` | Pause/Resume | |
| `monitors delete` | Delete → Confirm delete | |
| `maintenance add` | "Add window" | |
| `maintenance list` | windows list | |
| `maintenance rm` | Cancel → Confirm cancel | |
| `backup` | "Back up now" | |

## Deliberate exclusions (documented, not gaps)

- `/livez`, `/readyz`, `/version` — machine/readiness endpoints; surfaced as banners and
  via `ping`, not as clickable operator actions.
- Global `GET /api/incidents` view — per-monitor incidents exist; a fleet-wide incident
  list is a product decision, tracked here so it can't silently become "missing".
- Log/SMTP/webhook inspection — operator-only server config, by design not in the UI.
