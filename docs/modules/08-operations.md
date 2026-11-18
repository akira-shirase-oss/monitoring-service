# Module 08 — Operations (`scripts/`, deploy, guardrails)

**Status:** M4 complete: `SMON_MAX_DB_BYTES` ceiling, `WriteHealth` → 503 on failed
commits, `SMON_LOG_FILE` bounded rotation, 7-day retention → `check_stats_daily`,
`POST /api/backup` + `backup-drill.sh` restore proof, and `deploy/` +
`scripts/deploy.sh` — versioned releases with `current`-symlink upgrades/rollbacks.

## Purpose

Run the monitor as real software: bounded resources, survivable failures, rehearsed recovery —
not a process only the author can babysit.

## Contract

- **Storage guardrail:** ~2 GiB budget for the SQLite file set + logs. Approaching the cap is
  a warning state; at the cap, writes fail and the API/status say so (module 02 failure case).
  The ceiling is set per connection (`after_connect`), never once through the pool.
- **Bounded logs:** structured JSON (tracing), byte-capped rotation, dropped-count surfaced.
- **Retention:** module 02's expiry (D-012: raw checks, aggregates, incidents, windows,
  delivered outbox; monitor cap) runs as a scheduled job through the writer, in batches:
  each writer command handles at most `RETAIN_BATCH` (5,000) rows of each kind in its
  own transaction, so observations commit between batches instead of waiting out a long
  first pass (`an_observation_lands_while_a_large_retention_pass_is_still_running`).
- **Backup/restore:** consistent backup of `monitor.db` (WAL-aware) into `SMON_BACKUP_DIR`
  only, under server-drawn names — a client-chosen path is refused (D-019); restore drill
  proves open incidents and outbox rows survive; restore is documented and rehearsed
  before M4 exits.
- **Deploy:** the monitor runs off the target host (independence is the point); upgrade and
  rollback are scripted; the public status page is hosted elsewhere (module 07). The
  systemd unit runs as the system user `smon` with no capabilities and a read-only
  system except `data/` and `status/`; `TimeoutStopSec=40s` outlasts the SIGTERM drain
  (pinned by `tests/deploy_unit.rs`).
- **Config:** env-driven (`SMON_*`), localhost-bound by default, secrets never logged.
  Parsed once into a typed `Config` (`config.rs`) with ranges and cross-field rules
  (`SMON_STATUS_TTL_S` > `SMON_STATUS_INTERVAL_S`, the SMTP trio all-or-none); an
  invalid or blank value fails the boot listing every problem, unknown `SMON_*` names
  are warned about. `SMON_ADMIN_TOKEN` must be ≥ 32 bytes with no whitespace or `#`
  (a pasted template line or placeholder is not a credential). Pinned by
  `invalid_smon_values_fail_the_boot_and_name_every_problem`
  against the real binary.

## Failure cases

| Trigger | Required result |
|---|---|
| Disk full | writes fail; status unready; nothing half-written |
| Invalid `SMON_*` value | boot fails naming every bad variable; no store created, no work started |
| Writer or coordinator stops (panic, exit) | final unavailable beat → drain → exit non-zero → restarted by the service manager; the boot records the gap (D-011, D-014) |
| SIGTERM (systemd/launchd stop, `kill`) or SIGINT | same drain: readiness off, engine cancelled, open HTTP requests ≤ 12 s, engine tasks ≤ 15 s, exit 0 (`sigterm_and_sigint_drain_the_engine_and_exit_cleanly`, real binary) |
| Log volume explodes | rotation caps it; dropped count visible |
| Upgrade mid-incident | incidents + outbox survive; generation model holds |
| Restore from backup | consistent store; no replayed external effects as "fresh" |

## Testing parameters

- **Drills (scripts + assertions):** backup→destroy→restore keeps open incidents and outbox;
  disk-pressure fixture (small FS or quota) shows fail-safe, not corruption.
- **Upgrade rehearsal:** run v(n), upgrade binary, verify API + incidents intact.
- **Load:** 24 h soak measures storage growth vs the 2 GiB budget and log-rotation behavior.
