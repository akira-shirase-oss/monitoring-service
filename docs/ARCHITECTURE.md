# Architecture

One process, four duties, one store. The design's job is to keep *observing*, *remembering* and
*telling* separable — so a failure in one duty cannot silently corrupt another's evidence.

## Sources of truth

| Thing | Authority | Notes |
|---|---|---|
| Monitors, checks, incidents, notify outbox | SQLite (`monitor.db`, WAL) | The only durable state. Written by exactly one writer task. |
| Current status shown to users | Derived: open incident + latest coverage | Never a stored "green flag" — it is recomputed from evidence. |
| In-flight slots, streaks | Coordinator task memory | Streaks are rebuilt from stored checks on restart; open incidents from the table. |
| Public status page | Sanitized snapshot artifact | Generated, expiring, hosted separately. Never the dashboard itself. |

## Process model (target state; see ROADMAP for what exists today)

```
monitor-server
├── HTTP API + dashboard/static serving (axum, bounded bodies/timeouts)
├── coordinator task            — one; owns the schedule, claims slots, collects results
│   └── probe tasks             — bounded workers (global slots, per-origin caps)
├── db-writer task              — the ONLY writer; mpsc commands, one tx per observation
└── notify-deliverer task       — separate pool; reads the outbox, finite retries
```

The guide asks for one coordinator and bounded probe workers: a single task owns the schedule
because "one attempt per slot" is a serialization property, and probes fan out to short-lived
tasks because a 5-second HTTP deadline must not stall the next slot. The writer is one task so
that "commit observation + incident transition + outbox together" is a single transaction —
three writes or none.

## Request/data paths

- **Manage:** CLI/dashboard → `POST /api/monitors` → writer → coordinator reload watch.
- **Observe:** coordinator slot due → probe task (deadline, size bound, target policy) → result
  → writer command → one tx (check row + incident transition + outbox row) → status map → API.
- **Alert:** incident transition enqueues an outbox row in the same commit → deliverer task →
  webhook → delivery evidence (or next_attempt with backoff).
- **Display:** dashboard reads `/api/*`; the public status page reads only the sanitized snapshot.

## Consistency and failure model

- **Commit-before-tell:** nothing is displayed, alerted or counted until the writer confirms the
  transaction. A crash between probe and commit means the observation is simply absent —
  coverage shows a gap, not a fabricated outcome.
- **Generation invalidation:** edits/pauses bump `monitors.generation`; results carrying an
  older generation are dropped at the writer boundary and never update status.
- **Uncertainty is data:** `unknown` (missing coverage), `uncertain` (delivery without ack) and
  open incidents are all first-class. Retries may duplicate an external delivery but never a
  logical event.
- **Bounds (lab profile, all adjustable):** 100 monitors @ 60 s, 20 global probe slots,
  2 per origin, 1 in-flight per monitor, 64 KiB max probe body, 16 KiB max API body,
  10 s API deadline, 5 s probe deadline, 7-day raw check retention, ~2 GiB storage guardrail.

## Repo layout

`crates/monitor-domain` (pure contract) · `crates/monitor-db` (store + writer) ·
`crates/monitor-server` (API, coordinator, deliverer, static) · `crates/monitor-cli` (`monitor`)
· `apps/dashboard` (SvelteKit SPA) · `fixtures/` (fault servers) · `perf/` (k6 + results) ·
`docs/modules/` (per-module contracts) · `scripts/` (gates).

Ports: API `127.0.0.1:58400`, vite dev `:58600` (D-006).
