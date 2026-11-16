# Module 02 — Store (`crates/monitor-db`)

**Status:** single-writer task live (M1); retention live (M4.1, D-012): checks older than
`SMON_RETENTION_S` (default 7 d) roll into `check_stats_daily` and delete in one transaction;
an open incident keeps only its birth streak raw; aggregates, resolved incidents, ended
windows and delivered outbox rows have horizons too; `check_stats` unions both tables;
`SMON_MAX_MONITORS` caps the monitor count.

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
- **Retention (M4, D-012):** raw checks expire after 7 days into daily aggregates (365 days);
  resolved incidents and their outbox rows after 365 days; delivered outbox rows 30 days
  after delivery; ended windows after 365 days. Expiry never deletes an open incident, its
  birth-streak rows, or an undelivered outbox row of a kept incident. Tests:
  `an_open_incident_keeps_only_its_birth_streak_past_retention`,
  `retention_prunes_delivered_outbox_old_incidents_aggregates_and_windows`.
- **Corrupt rows are errors, not defaults:** `MonitorRow::try_from` revalidates stored values;
  a violation returns `Corrupt`, never a guessed monitor.

## Failure cases

| Trigger | Required result |
|---|---|
| Disk write fails | commit fails → `WriteHealth` consecutive-failure counter grows; `/readyz` answers 503 `unavailable`, served statuses read `unknown` and the snapshot beat is born expired (D-011); the counter resets only when a write lands — never paint unsaved results green |
| Writer task dies | the supervisor sees the exit → `/readyz` 503 `writer exited`/`writer panicked`, statuses `unknown` (`a_dead_writer_makes_the_monitor_report_unavailable`) |
| Store at its cap | retention still runs: it copies expiring rows to a `TEMP` table, deletes, then upserts aggregates into the freed pages — a full store recovers on the next pass (`retention_frees_space_in_a_store_filled_to_its_cap`) |
| Full disk | admission/write fails safely and visibly (module 08 guardrails). The `SMON_MAX_DB_BYTES` ceiling is `PRAGMA max_page_count`, which SQLite scopes to one connection — it is applied in the pool's `after_connect` hook, so every pooled connection (including ones reopened after idle/lifetime expiry) carries it; `a_capped_store_refuses_growth_on_every_connection` holds all four at once |
| Restart | open incidents reload; streaks rebuild from stored outcomes |
| Restart after downtime longer than one interval + grace | one `missed` row per monitor at the first lost slot (D-014): streaks reset, status `unknown` until a fresh probe (`downtime_is_a_gap_failures_either_side_are_not_consecutive`) |

## Testing parameters

- **Unit/integration:** tx atomicity (inject failure between writes → no partial state);
  generation-aware result acceptance; `open_incident` round-trip.
- **Fault:** kill the process between probe and commit → observation absent, coverage shows gap
  (process-kill test, module 09).
- **Load:** 100 monitors × 60 s ≈ 1.7 writes/s sustained — trivially inside budget; measure
  commit p95/p99 during the 24 h soak; storage growth recorded against the ~2 GiB guardrail.
