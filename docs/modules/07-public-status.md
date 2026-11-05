# Module 07 — Public status + heartbeat (`status` snapshot, renderer, external check)

**Status:** M3. Not started.

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
