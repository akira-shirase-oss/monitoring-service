# Decisions

Newest first. `D-NNN` ids are permanent; referenced from code and build-log entries.

## D-018 — Slots are spread by phase; the probe budget follows the schedule, up to 512

The coordinator starts probes on its 1 s tick within a global bound. That bound was a
fixed 20, and every new monitor was anchored one tick out, so monitors created together
fell due on the same tick for ever: a wave of 200 got about 100 starts inside the 5 s grace
and the rest were missed (the 2026-11-17 ceiling probe: 800 pass, 800 missed), and the
starts it got were up to 5 s late — most of the measured commit lag.

- **Phase.** Each monitor's slots sit at `phase (mod interval)` in Unix milliseconds; the
  phase comes from its id (`splitmix64` over the UUID's halves). New and edited monitors
  start on that grid at least one tick out — so a new monitor's first check comes within
  one interval, not one tick — and a restart keeps the grid.
- **Budget.** Global slots = `Σ (tick + timeout) / interval` over unpaused monitors (per
  tick `Σ tick/interval` starts, each holding its slot at worst for its timeout), never
  below the configured floor (20) nor above **`MAX_GLOBAL_SLOTS` = 512** — outbound
  sockets, well inside the unit's 4096 descriptors beside `SMON_MAX_CONNECTIONS`. The
  coordinator resizes its semaphore whenever the registry changes. At 15 s/3 s that is
  1,920 monitors; at the 60 s/10 s defaults, about 2,790.
- **Refusal.** A create or edit whose resulting schedule needs more than 512 slots, or
  more starts per tick on one origin than its 2 per-origin slots (more than 30 monitors
  at 15 s on one host:port), is refused with 409 `capacity_exceeded` naming the bound —
  never accepted and then missed in silence. The check and its write run under one lock.
  An origin is the **resolved address and port**, not the URL's spelling (`origin.rs`):
  `localhost:9` and `127.0.0.1:9` are one origin, for the per-origin bound and for this
  check. Names are resolved through a process-wide cache (5 min; the coordinator refreshes
  every monitor's name in the background, a write resolves its own within 3 s); a name
  not resolved yet keys by its spelling.
  Slow targets can still miss slots (a probe holds its slot up to its timeout); those
  misses are counted, as always.
- **Writer connection.** The single writer commits on a connection of its own instead
  of drawing from the API's 4-connection pool, where under concurrent reads each commit
  queued for a connection (70-96 ms with 32 readers against 1-3 ms on its own).

Pinned by `monitors_created_together_fall_due_spread_across_their_interval`,
`sixteen_hundred_monitors_on_one_interval_miss_nothing`,
`a_schedule_the_coordinator_cannot_serve_is_refused_by_name` and
`the_writers_commits_never_queue_behind_api_reads`.

## D-017 — The webhook URL is operator configuration, not SSRF-guarded (P0 before tenants)

Probes go through the SSRF guard (`ProbePolicy` plus the validating resolver): http(s)
only, every non-global address refused, DNS answers checked, every redirect hop
rechecked. The alert webhook does not. `SMON_WEBHOOK_URL` only has to be http(s) with a
host, so loopback, private, link-local and metadata addresses are accepted, and its DNS
answer is not validated (redirects are not followed). That is deliberate
for a self-hosted monitor: only the operator writes `env`, they can reach those addresses
anyway, and an internal alert relay is a normal receiver — so it is not a privilege
boundary today.

It becomes one the moment someone other than the operator can set a webhook URL. **Before
tenants can (SaaS S1/S2, per-workspace alert channels), this is a P0 gate:** a tenant's
URL goes through `ProbePolicy` and the validating resolver when it is saved and again at
every send, with no redirects, exactly as a probe target does (docs/SAAS_PLAN.md, Alert
channels).

## D-016 — Session rows hold a token-bound digest, never the cookie's id

The `sessions` table used to store the cookie's id as is, so the store — and every
`VACUUM INTO` backup of it — held working credentials for up to `SMON_SESSION_TTL_S`, and
rotating `SMON_ADMIN_TOKEN` (the operator's way to revoke access) left every dashboard
session working until it expired.

The cookie still carries a random id (UUIDv4); the row is keyed by
`hex(SHA-256("smon-session-v1\0" ‖ SHA-256(SMON_ADMIN_TOKEN) ‖ id))`
(`auth::session_key`). The digest is one-way, so a copied store or backup can't be
replayed as cookies, and it is bound to the token, so after a rotation every outstanding
cookie maps to a key no row has: each session ends at once, and the dashboard asks for
the new token. Migration 0008 deletes the raw-id rows written before this (everyone signs
in again once). Pinned by `rotating_the_admin_token_ends_every_session` and
`the_store_and_its_backups_hold_no_usable_session_id`.

A session also ends when it goes unused for `SMON_SESSION_IDLE_S` (default 1 h), inside
its absolute lifetime (`SMON_SESSION_TTL_S`, 12 h). Use refreshes `last_seen_at`
(migration 0009) at most once a minute, so the dashboard's 15 s refresh is not a write per
request, and a failed refresh (a store at its byte cap) still authorizes. An open tab keeps
its session alive; a forgotten one ends within the hour. Pinned by
`an_idle_session_ends_and_use_keeps_it_alive` (store) and
`an_unused_session_ends_at_the_idle_limit_and_use_keeps_it_alive` (API).

## D-015 — A normal start requires an existing store; only `--init` creates one

The store is meant to live on its own volume (the SaaS droplet uses a DigitalOcean volume).
If the server starts before that volume is mounted, `/opt/smon/data` is just the bare
mountpoint directory, and `create_if_missing` used to create a fresh, empty store there:
the monitor booted "healthy" with no monitors and no history, and wrote new evidence onto
the root disk, hidden under the volume once it mounted.

`monitor-server` (no arguments) now opens the store without creating it and refuses to
boot if the file is missing or has no tables, naming the path and the likely cause (the
volume is not mounted). `monitor-server --init` is the only way to create a store: it
creates and migrates it, then exits without serving — the install step (RUNBOOK) and the
first step of every harness that starts from scratch. `--version` prints the version and
`--help` the usage (both exit 0); any other argument is a usage error (exit 2), so a mistyped flag never starts a server
with default settings. The unit's `RequiresMountsFor=` covers the ordering; this covers
a mount that is configured differently, or not at all.

## D-014 — Downtime is one stored `missed` row per monitor

D-007 made every slot a stored row so rebuilds replay gaps as gaps — but slots that
passed while the *process* was down produced no row at all. The boot rebuild then replayed
the last pre-outage observations as current (the first snapshot after a long outage said
"up"), and failures before and after the outage counted as one consecutive streak.

At boot, before the coordinator or the first snapshot beat, every unpaused monitor whose
newest stored slot is older than one interval plus the dispatch grace gets one `missed`
row, at the first slot that never ran, committed through the writer: streaks reset, the
status reads `unknown` until a fresh probe, and the outage is visible in the record. One
row per gap, not one per lost slot — coverage already derives expected slots from elapsed
time, and back-filling hundreds of rows is the catch-up storm the schedule refuses
elsewhere. A restart inside one interval writes nothing. Pausing and unpausing is not
covered here (the paused period is the operator's choice, not lost evidence).

## D-013 — Refused literals are rejected at registration; probe-time refusals page

The SSRF guard refuses local/private/reserved targets before any socket opens. Before
D-013 a loopback or private *literal* could be registered under the strict profile and
then failed every slot — the monitor reporting its own policy as the target being down.

- **At registration:** create (and a PATCH that sets a URL) runs the same
  `ProbePolicy::check` the probe runs; under the strict profile a refused literal answers
  422 `refused_target` and is never stored. `SMON_PROFILE=lab` is the explicit opt-in.
- **At probe time:** a refusal stays a **target failure** — `transport` + detail
  `rejected_local` — and pages like any other. This covers a public name whose DNS answer
  turned private (parked at 127.0.0.1, hijacked) and a redirect hop to a local *name*
  (`http://localhost/…`, refused by the resolver). Both are real outages for every
  visitor of the target.

*Revised 2026-11-16.* The first version of D-013 committed probe-time refusals as
`missed`. That turned exactly those two outages into a silent `unknown` that never paged —
the opposite of this project's promise — and was reverted. A redirect to a refused
*literal* stays what it always was: the 3xx is the target's own answer, `http_fail`
evidence. Pinned by `a_redirect_to_a_local_name_is_a_failure_that_pages` and
`a_name_that_resolves_to_a_local_address_is_a_failure_that_pages`.

## D-012 — Every table has a horizon; the monitor count has a cap

Retention used to exempt *every* raw row of a monitor with an open incident (one
long-open incident meant unbounded raw history), and never pruned the outbox, incidents,
aggregates or windows; nothing bounded the number of monitors. At 1 row per slot the
store's growth must be a function of configuration, not of uptime.

| Record | Horizon (default) | Variable | Counted from |
|---|---|---|---|
| raw `checks` | 7 days, then rolled into `check_stats_daily` | `SMON_RETENTION_S` | `slot_at` |
| open incident's birth streak | while the incident is open | — | rows `first_fail_at`…`opened_at` (≤ `failures_to_open` ≤ 10) |
| `check_stats_daily` | 365 days | `SMON_AGGREGATE_RETENTION_S` | `day` |
| resolved incidents + all their outbox rows | 365 days | `SMON_INCIDENT_RETENTION_S` | `resolved_at` |
| ended maintenance windows | 365 days | `SMON_INCIDENT_RETENTION_S` | `ends_at` |
| delivered outbox rows | 30 days | `SMON_OUTBOX_RETENTION_S` | `delivered_at` |
| monitors | cap 100 (create → 409 `limit_reached`) | `SMON_MAX_MONITORS` | — |

The birth streak is the incident's evidence: why it opened. Raw rows written *while* it is
open are history like any other and age into aggregates, so coverage and success stay
exact. Never pruned: open incidents, and undelivered (pending or exhausted) outbox rows of
kept incidents — they remain the visible "no receiver" signal until their incident ages
out. A pass is a series of bounded transactions through the single writer (at most
5,000 rows of each kind per command, so observations interleave between them); deleting an incident deletes its
outbox rows first (FK), and every delete runs before the aggregates are written (the
expiring rows wait in a `TEMP` table), and its transaction may allocate up to 256 pages
past the cap for the summaries (restored on the same connection afterwards), so a pass
can run in a store already at `SMON_MAX_DB_BYTES`. The 256 pages are an absolute
allowance above the cap (`max(cap + 256, size)`), not 256 more on every pass, so the
effective cap cannot ratchet upward. The cap is one conditional `INSERT … WHERE COUNT(*) < cap`, so
concurrent creates cannot overshoot it; paused monitors count (they keep history).

## D-011 — One engine verdict: a monitor that is not observing serves `unknown`

The committed status map only moves when the writer commits. If the writer task dies, the
coordinator dies or wedges, or every commit fails, the map freezes at its last value — and
the API, the dashboard and the public heartbeat used to keep serving that frozen "up" as
current, because nothing watched the tasks (their `JoinHandle`s were dropped) and
`/readyz` only saw write failures.

`vitals::Vitals` is now the single verdict, read by `/readyz`, the statuses `/api/monitors`
serves (with the verdict beside them as `monitoring`), the public snapshot, and through the
API the dashboard. Inputs: the writer's consecutive failed-*observation*-commit counter
(retention and delivery bookkeeping don't count — a failed retention pass marks the
monitor degraded until a pass succeeds), a coordinator
heartbeat stamped on every loop pass (stalled after 30 s of silence), exits of supervised
tasks, and the drain flag set at shutdown. `vitals::Supervisor` owns every engine task's
`JoinHandle`; an exit outside shutdown — return, panic or abort — is logged once and
recorded.

- **unavailable** — not observing: writer or coordinator stopped, coordinator stalled,
  store writes failing, or shutting down. `/readyz` 503, every served status `unknown`,
  each snapshot beat written born-expired with `monitoring: "unavailable"`, dashboard
  banner naming the cause.
- **degraded** — observing, but a supporting task (a deliverer, the snapshot writer,
  retention) stopped, or the last retention pass failed. `/readyz` **200** with
  `"status":"degraded"` and the faults (the monitor still observes and serves; only
  `unavailable` takes it out of rotation); `monitor ping` exits 5. Statuses stay as
  committed.
- **ok** — everything running; `/readyz` 200.

Statuses are not rewritten in the store: `unknown` is a property of *serving* while the
engine is down, and the committed history stays exactly what was observed.

*Amended 2026-11-16.* Honest is not enough: a process that stays up without its writer
or coordinator never heals, and the service manager never restarts it. When an
**observing** task stops outside shutdown the supervisor raises an engine failure;
serving stops, one born-expired `monitoring: unavailable` beat is written at once, the
engine drains, and the process exits non-zero, so systemd `Restart=on-failure` or launchd
`KeepAlive` restarts it and the boot records the gap (D-014). A coordinator that is alive
but makes no loop pass for 2 minutes (4 × the 30 s stall threshold) is treated the same
way — a stall that long will not clear by itself. A freeze of the whole process
(SIGSTOP, a VM pause) is not a stall: when the watchdog's own previous check is more than
the limit ago, it skips that round and lets the coordinator make its first pass. That
first pass commits one `missed` row per lost slot (962 for 100 monitors after a 130 s
freeze), so the coordinator also beats after every committed miss: staleness is judged by
progress, and a long catch-up is never read as a wedged loop (a writer that stops taking
commits stops the beats). (Unit-tested by
`committing_a_backlog_of_misses_keeps_the_heartbeat_fresh`; the 130 s SIGSTOP drill not
re-run since.) Supporting tasks stay *degraded*, with
no exit. The systemd unit backs restarts off from 15 s to 10 minutes (`RestartSteps`,
systemd ≥ 254) under a start limit of 10 in 30 minutes that a backed-off loop never
reaches: a deterministic crash is spaced out instead of looping every few seconds.
*Corrected 2026-11-16:* this used to say a late volume is retried until it appears. It
is not — a `RequiresMountsFor=` mount whose job fails is a dependency failure, outside
`Restart=`, and the unit waits for a manual start (RUNBOOK, "The data volume attached
late"). Older systemd
keeps the fixed 15 s and leaves the unit `failed` after ~2.5 minutes of failures. Pinned by `an_observing_task_stopping_ends_serving_as_an_engine_failure`
and `a_supporting_task_stopping_keeps_serving`.

## D-010 — Certificate trust is deterministic: webpki-roots, never the local store

A monitor should report what a *public* client sees. Reqwest's default on macOS delegates
verification to `rustls-platform-verifier` → Security.framework, which flattens every
certificate failure into an opaque OSStatus and silently trusts whatever roots are in the
operator's keychain — meaning a corp-rooted laptop can report "down" or "up" for reasons no
visitor shares. The probe client instead builds its own `rustls::ClientConfig` on
`webpki-roots` (same root program Firefox ships) via `tls_backend_preconfigured`: rich
`CertificateError` variants survive into `classify`, behavior is identical on every OS, and
the answer means the same thing to every reader. Operators with internal PKI opt in
explicitly through `ProbePolicy.extra_roots` — a configured root is a stated assumption,
not an accident of where the monitor happens to run.

## D-009 — Edit and invalidation are one write: `generation` bumps atomically

Every management write (`PATCH`, pause/unpause) revalidates the merged monitor through the
same rules as a create (`MonitorPatch::apply` → `validate_fields`), then one `UPDATE` sets the
fields *and* `generation = generation + 1` conditioned on the generation read. A late probe
result claimed under the old generation is dropped by the writer — the edit and the
invalidation are a single committed fact, so there is no interleaving where the new config
takes the old probe's outcome. A client-supplied `generation` is an optional optimistic
guard; a mismatch answers `conflict` rather than last-writer-wins.

## D-008 — Monitors with evidence cannot be deleted

`DELETE` is a single conditional statement: `DELETE … WHERE NOT EXISTS (checks|incidents)`.
Atomic, so a probe committing between "has history?" and "delete" can't orphan evidence.
Monitors with recorded checks or incidents get `conflict` — pause stops monitoring without
erasing the record. The alternative (cascade delete) would let an API call destroy the
incident history the whole system exists to prove.

## D-007 — Missed slots are stored `checks` rows, not gaps

The scaffold's schema comment said a missed slot would be "visible by the absence of a row."
That model breaks restart rebuild: the rebuild replays stored outcomes, and two failures
separated by an unstored miss would replay as consecutive — fabricating an incident that never
opened. Every slot now produces a row (`outcome = 'missed'` for slots that never ran), so
coverage, streak rebuild and replay all read the same history. Coverage % = non-missed /
total rows.

## D-006 — Ports: 58400 for the server, 58600 for the dashboard's dev server

`SMON_LISTEN` defaults to `127.0.0.1:58400`; vite dev runs on `127.0.0.1:58600` and proxies
`/api`, `/livez`, `/readyz`. Picked adjacent to Colophon's 58000/58200 block without colliding.

*Amended 2026-11-16.* 58400 belongs to an installed monitor. Scripts that start their own
server use their own ports so they can run beside one: `scripts/dev.sh` on `SMON_DEV_PORT`
(default 58465; vite proxies there), `scripts/backup-drill.sh` on `SMON_DRILL_PORT` /
`SMON_DRILL_FIXTURE_PORT` (58460/58461). Each refuses to start if its port already answers
and stops only processes it started itself, by PID (`scripts/lib-ports.sh`) — the drill
once `kill -9`ed whatever held :58400.

## D-005 — Provisional status semantics (the `Health` state machine)

Implemented in `monitor-domain`: three consecutive failures open an incident, two fresh passes
confirm recovery, a missed slot resets both streaks and returns the display to `unknown`, and an
open incident survives restarts until evidence resolves it. Sharpen this when the coordinator
lands — in particular whether an open incident during coverage gaps displays `down` or a
separate "last known down, monitoring stale" state.

## D-004 — shadcn-svelte components live inside `apps/dashboard`

No `packages/ui` yet: with one app, a shared package is indirection without a customer. When the
independently hosted public status renderer arrives, extract shared pieces then — not before.
`components.json` is configured so `npx shadcn-svelte add <component>` drops new components in
`src/lib/components/ui`.

## D-003 — SQLite via sqlx, WAL on, foreign keys on; pool now, dedicated writer later

`monitor-db` opens the file with `create_if_missing`, `journal_mode=WAL`, `foreign_keys=ON`.
The guide calls for a dedicated database worker so writes serialize and never stall probe
workers; that task boundary arrives with the coordinator. Until then handlers use the pool
directly — bounded at 4 connections.

## D-002 — Layout and conventions mirror Colophon

`crates/` (domain, db, server, cli), `apps/`, `packages/`, `scripts/`, `docs/`, `fixtures/`,
`perf/`; same toolchain pin (1.99.1), lint set, vite-config-driven SvelteKit, Tailwind v4 +
shadcn-svelte ("sera" style), npm workspaces, and the Rust rules (typed errors in libs,
`anyhow` only in binaries, every I/O await bounded, nothing external unwrapped).

## D-001 — The old implementation's baseline is unavailable

The rebuild guide's step 1 ("preserve the old repository; port the black-box failure fixtures")
cannot be done: the previous code is not on this machine or in the workspace archive, and the
maintainer confirms it is gone. Consequences:

- The contracts in `02_Service_Monitor_Rebuild.docx` and the build-log entries on
  404human.dev/service-monitor are the effective specification.
- The failure fixtures must be rebuilt from scratch (`fixtures/`, plus a Node fault server as
  the guide describes). Until they exist, "the rewrite preserves known behavior" cannot be
  claimed — say "rebuilt contract" instead.
- If the old repository resurfaces, archive it under `references/` unchanged and port its
  fixtures before touching internals.
