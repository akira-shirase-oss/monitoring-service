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
- **Stable `event_id`** (uuid v7) per logical event; dedup/idempotency key on delivery.
- **Separate delivery pool:** deliverer task(s) read the outbox; probes never wait on them.
  A hung receiver occupies a delivery slot with its own deadline, nothing else.
- **Finite retries:** bounded attempts (default 8), exponential backoff with full jitter,
  per-attempt deadline; after exhaustion the event stays `undelivered` and is reported —
  never silently dropped.
- **Uncertainty is explicit:** a timed-out delivery is `uncertain` — retried under the same
  `event_id`, so a receiver may see a duplicate but never a second *logical* event.
- **Channels:** webhook first (HMAC-signed body, bounded); email later behind an adapter.

## Failure cases

| Trigger | Required result |
|---|---|
| Receiver hangs | deadline; retry later; probes unaffected |
| Crash after commit before delivery | outbox row survives; delivered on restart |
| Receiver 500s / flaps | bounded retries, then visible `undelivered` |
| Deliverer dies | probes + incidents continue; backlog drains on restart |

## Testing parameters

- **Unit:** backoff schedule bounds (max delay cap, jittered ≥0); outbox state transitions.
- **Integration:** receiver endpoint fixtures — immediate 200; 3×500 then 200 (delivered,
  attempts recorded); permanent 500 (gives up after N, marked undelivered); hanging receiver
  (attempt deadline hit, event still delivered later).
- **Process-kill:** kill between commit and delivery → exactly-once *logical* event on restart.
- **Load:** incident storms — 20 monitors failing together enqueue 20 events; delivery drains
  with receivers at 200 ms latency without blocking probes (soak assertion).
