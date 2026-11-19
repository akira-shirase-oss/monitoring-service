# Module 04 — Incidents + notification delivery (`monitor-server` deliverer, `notify_outbox`)

**Status:** built (M2). Outbox deliverer live: bounded attempts, deterministic FNV jitter,
HMAC-SHA256 `x-smon-signature` + stable `x-smon-event`, delivery bookkeeping through the
single writer (`mark_delivery`), exhausted events stay visible in the table.

## Purpose

Turn committed incident transitions into exactly one logical event each, delivered
at-least-once to a receiver — without alerting ever being able to stall observing.

## Contract

- **Enqueue in the same commit** as the check row and incident transition — an undelivered
  alert is durable work, not a log line.
- **Four incident times, committed separately (M6.3):** `first_fail_at` (start of the
  failing streak — derived inside the commit transaction from committed check rows, the
  oldest consecutive failure since the last `pass`/`missed`), `opened_at` (the slot that
  crossed `failures_to_open`), `last_fail_at` (re-stamped on every failing check while
  open), `resolved_at` (recovery). Start-of-failure ≠ time-of-notice is the forensic
  point; pre-M6.3 rows keep NULL rather than fabricated history. All four ride the API
  `IncidentView` and the webhook `incident` reference.
- **Stable `event_id`** (uuid v7) per logical event; dedup/idempotency key on delivery.
- **Separate delivery pool:** deliverer task(s) read the outbox; probes never wait on them.
  A hung receiver occupies a delivery slot with its own deadline, nothing else.
- **Finite retries:** bounded attempts (default 8), exponential backoff with full jitter,
  per-attempt deadline; after exhaustion the event stays `undelivered` and is reported —
  never silently dropped.
- **Uncertainty is explicit:** a timed-out delivery is `uncertain` — retried under the same
  `event_id`, so a receiver may see a duplicate but never a second *logical* event.
- **Channels (M7.1):** `notify_outbox.channel` selects the transport — `webhook` always,
  `email` when SMTP is configured, `tenant_webhook` while the row's workspace keeps a
  webhook (S1d). One transition enqueues one row *per configured channel* (same
  incident/kind, distinct `event_id`s — each channel owns its delivery bookkeeping).
  Each deliverer polls only its own channel, so a dead SMTP relay can never stall
  webhooks or vice versa. No configured channel ⇒ no dead-plan rows.
- **Tenant webhook channel (S1d, D-017):** each workspace may store its own
  `webhook_url` + `webhook_secret` (`PUT/GET/DELETE /api/workspace/webhook`,
  member work; the secret signs `x-smon-signature` like the operator's does). The URL
  is SSRF-guarded at save (`ProbePolicy::check_resolved`) and at every send — the
  strict client re-resolves through `PolicyResolver` inside the connect, re-runs
  `check` for IP literals, follows no redirects, honours `attempt_deadline`, and never
  reads the response body. A send-time refusal lands on the row as
  `notify_outbox.last_error` (`rejected_local` & friends) and the payload is never
  sent. The tenant deliverer always runs; with no workspace configured it is an idle
  poll.
- **Email sink:** `Sink::Email` — lettre `AsyncSmtpTransport` on rustls+tokio
  (`SMON_SMTP_DSN`: prefer `smtps://user:pass@host[:465]` — implicit TLS, no cleartext
  preamble; `smtp://…?tls=required` for mandatory STARTTLS where implicit TLS isn't
  offered; bare `smtp://` only for an explicitly plaintext local relay),
  `SMON_SMTP_FROM`, `SMON_SMTP_TO` (comma-separated). All three required together —
  partial config is a boot error, not a half-live channel. Subject carries
  `[service-monitor] <monitor>: <kind>` with CR/LF stripped (header injection never
  reaches the wire); `Message-ID: <event_id@service-monitor>` threads retries. A positive
  2xx/354-close reply is success; anything else is the same deterministic retry/exhaustion
  path as webhooks. Credentials live in env only and are never logged — `Sink::describe`
  reports the recipient count, not addresses.
- **No proxies:** the webhook client ignores proxy environment variables and dials the
  receiver directly; SMTP (lettre) never used them. Egress proxies are not supported.
- **The operator webhook URL is not SSRF-guarded** (D-017): `SMON_WEBHOOK_URL` is
  operator configuration — http(s) with a host, nothing more, so an internal relay
  works. Tenant URLs *are* guarded (the bullet above); the scope split is the D entry.
- **No redirects:** the webhook client follows none. A 307/308 would re-send the payload
  and its signature wherever the receiver points; a 3xx is a failed attempt, retried with
  backoff like a 5xx (`a_receivers_redirect_is_not_followed_and_nothing_is_re_sent`).
- **URL redaction (`redact.rs`):** chat webhooks carry their credential in the path
  (Slack `/services/T…/B…/<token>`, Discord `/api/webhooks/<id>/<token>`) or a query
  token, so stripping userinfo is not redaction. Anything logged shows the webhook as
  `scheme://host[:port]` only (`redact::origin`). The monitored URL inside an alert — the
  webhook payload's `monitor.url` and the email body — keeps scheme, host, port and path
  and drops userinfo, query and fragment (`redact::for_alert`): an alert's readers are a
  channel or an inbox, not the monitor's admins. Pinned by
  `webhook_secrets_in_the_path_or_query_never_reach_the_logs` (captures the real log
  output) and `alert_payloads_strip_credentials_from_the_monitor_url`.

## Failure cases

| Trigger | Required result |
|---|---|
| Receiver hangs | deadline; retry later; probes unaffected |
| One channel's relay down | other channel's rows still drain; failing channel retries under its own event_id |
| Crash after commit before delivery | outbox row survives; delivered on restart |
| Receiver 500s / flaps | bounded retries, then visible `undelivered` |
| Tenant URL rebinds private at send | refused inside the connect; `rejected_local` on the row; payload never sent |
| Tenant channel cleared after enqueue | row answers `no_target` — visible, never a silent drop |
| Deliverer dies | probes + incidents continue; backlog drains on restart |

## Testing parameters

- **Unit:** backoff schedule bounds (max delay cap, jittered ≥0); outbox state transitions.
- **Integration:** receiver endpoint fixtures — immediate 200; 3×500 then 200 (delivered,
  attempts recorded); permanent 500 (gives up after N, marked undelivered); hanging receiver
  (attempt deadline hit, event still delivered later).
- **Process-kill:** kill between commit and delivery → exactly-once *logical* event on restart.
- **Load:** incident storms — 20 monitors failing together enqueue 20 events; delivery drains
  with receivers at 200 ms latency without blocking probes (soak assertion).

## Maintenance windows (M6.11)

Schedule-scoped suppression so planned work doesn't page. A `maintenance_windows` row
(`starts_at`, `ends_at`, `reason`) is read **inside the observation gate** — the same
committed read that returns generation and thresholds — so creating or deleting a window
takes effect on the next slot, never cached.

- **Suppression silences the page, never the evidence.** A `failures_to_open` crossing
  inside an active window yields `Effect::IncidentSuppressed`: the check row commits, no
  incident row, no outbox event. The domain keeps counting the streak — a target still
  broken at expiry opens on the *next* failing check, not a fresh streak later.
- **Windows never block healing.** An incident open before the window still recovers on
  fresh passes inside it — the paged human gets their resolve.
- **`active` is computed server-side** in `MaintenanceView` and at the gate — no client
  re-derives "is this suppressing now".
- **Management surface:** `POST|GET /api/monitors/{id}/maintenance`,
  `DELETE /api/maintenance/{id}` (204 vs 404 honestly); CLI `monitor maintenance
  add|list|rm` (`--from now` for the common case).
