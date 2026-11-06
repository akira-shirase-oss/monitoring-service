# Module 07 — Public status + heartbeat (`status` snapshot, renderer, external check)

**Status:** built (M3, M6.7). `monitor-server` writes `status.json` every `SMON_STATUS_INTERVAL_S`
(default 30 s) into `SMON_STATUS_DIR` — sanitized (sha256-alias, status,
`incident_open_since` only), atomic (tmp + rename), self-expiring (`SMON_STATUS_TTL_S`,
default 90 s). `apps/status/index.html` renders it with zero runtime dependency — the
canonical independently-hosted page. The server also serves the same file unmodified at
`GET /status.json` (outside `/api`, no token), and the dashboard's `/status` route renders
it in-app (M6.7) — identical artifact, identical expiry semantics.
`fixtures/status-check.mjs` is the independent checker: expired/missing snapshot →
"unavailable", never "targets down".

## Purpose

Let users of the monitored services check availability without a login — and let them tell
"your thing is down" apart from "the monitor itself is down".

## Contract

- **Sanitized snapshot:** a generated artifact containing per-service status, incident
  summaries and generated/expiry timestamps — no monitor names, internal URLs, check details
  or ids beyond a stable public alias. TTL: snapshot declares its own freshness window.
- **Independently hosted:** the status page must render when monitor-server is dead — separate
  hosting from the probe host, static assets, zero runtime dependency on the API.
- **Heartbeat:** the monitor emits a periodic beat (file/endpoint the independent checker
  watches); when beats expire, status says "monitoring unavailable" — never "all targets
  down", and never a silent freeze.
- **Read-only:** no mutations through the status surface. Ever.
- **Separation from dashboard:** the public page is a different artifact from the dashboard;
  it never embeds management data.

## Failure cases

| Trigger | Required result |
|---|---|
| Monitor host down | heartbeat expires → "monitoring unavailable" |
| Snapshot stale | page shows its age; expired snapshot warns explicitly |
| Monitor restarted, no fresh data | services show `unknown`, not guessed states |

## Testing parameters

- **Unit:** snapshot sanitizer (internal fields cannot leak — schema-checked allowlist);
  freshness/TTL logic.
- **Integration:** kill the monitor → independent checker's view flips to unavailable within
  the heartbeat TTL; restored → recovers.
- **Load:** status page is static — verify it tolerates direct k6 at 300 rps served from a
  plain static host (it should be boring).
