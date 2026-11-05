# Module 08 — Operations (`scripts/`, deploy, guardrails)

**Status:** guardrails + backup live (M4.1–M4.3): `SMON_MAX_DB_BYTES` page ceiling,
`WriteHealth` → `/readyz` 503 on failed commits, `SMON_LOG_FILE` byte-bounded rotation,
7-day retention rolling into `check_stats_daily`, `POST /api/backup` (`VACUUM INTO`)
with `scripts/backup-drill.sh` proving open incidents + outbox survive a restore.

## Purpose

Run the monitor as real software: bounded resources, survivable failures, rehearsed recovery —
not a process only the author can babysit.

## Contract

- **Storage guardrail:** ~2 GiB budget for the SQLite file set + logs. Approaching the cap is
  a warning state; at the cap, writes fail and the API/status say so (module 02 failure case).
- **Bounded logs:** structured JSON (tracing), byte-capped rotation, dropped-count surfaced.
- **Retention:** module 02's 7-day raw + aggregates expiry runs as a scheduled job — bounded
  batches, never blocking the writer on live data.
- **Backup/restore:** consistent backup of `monitor.db` (WAL-aware); restore drill proves open
  incidents and outbox rows survive; restore is documented and rehearsed before M4 exits.
- **Deploy:** the monitor runs off the target host (independence is the point); upgrade and
  rollback are scripted; the public status page is hosted elsewhere (module 07).
- **Config:** env-driven (`SMON_*`), localhost-bound by default, secrets never logged.

## Failure cases

| Trigger | Required result |
|---|---|
| Disk full | writes fail; status unready; nothing half-written |
| Log volume explodes | rotation caps it; dropped count visible |
| Upgrade mid-incident | incidents + outbox survive; generation model holds |
| Restore from backup | consistent store; no replayed external effects as "fresh" |

## Testing parameters

- **Drills (scripts + assertions):** backup→destroy→restore keeps open incidents and outbox;
  disk-pressure fixture (small FS or quota) shows fail-safe, not corruption.
- **Upgrade rehearsal:** run v(n), upgrade binary, verify API + incidents intact.
- **Load:** 24 h soak measures storage growth vs the 2 GiB budget and log-rotation behavior.
