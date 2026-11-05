# Module 02 — Store (`crates/monitor-db`)

**Status:** single-writer task live (M1); retention arrives in M4.

## Purpose

SQLite is the authority for configuration, checks, incidents and the notification outbox.
Local file, WAL mode, foreign keys on — no server database, no Redis (guide decision).

## Contract

- **One writer.** A dedicated `db-writer` task owns all writes; commands arrive over a bounded
  mpsc channel and each observation is committed in **one transaction**: check row + incident
  transition + outbox row. Three writes or none.
- **Schema (migration 0001):** `monitors` (generation, paused, schedule), `checks` (one row per
  slot, `outcome = 'missed'` when a slot never ran — D-007), `incidents` (partial unique
  index — at most one open incident per monitor), `notify_outbox` (stable `event_id` unique).
- **Rebuild, don't trust memory:** on boot, open incidents come from the table and streaks are
  rebuilt by replaying each monitor's latest outcomes.
- **Retention (M4):** raw checks expire after 7 days; aggregates keep bounded history. Expiry
  must never delete rows referenced by an open incident or an undelivered outbox entry.
- **Corrupt rows are errors, not defaults:** `MonitorRow::try_from` revalidates stored values;
  a violation returns `Corrupt`, never a guessed monitor.

## Failure cases

| Trigger | Required result |
|---|---|
| Disk write fails | stop authoritative updates; `/readyz` reports unready; never paint unsaved results green |
| Full disk | admission/write fails safely and visibly (module 08 guardrails) |
| Restart | open incidents reload; streaks rebuild from stored outcomes |

## Testing parameters

- **Unit/integration:** tx atomicity (inject failure between writes → no partial state);
  generation-aware result acceptance; `open_incident` round-trip.
- **Fault:** kill the process between probe and commit → observation absent, coverage shows gap
  (process-kill test, module 09).
- **Load:** 100 monitors × 60 s ≈ 1.7 writes/s sustained — trivially inside budget; measure
  commit p95/p99 during the 24 h soak; storage growth recorded against the ~2 GiB guardrail.
