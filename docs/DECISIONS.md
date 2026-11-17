# Decisions

Newest first. `D-NNN` ids are permanent; referenced from code and build-log entries.

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
first step of every harness that starts from scratch. `--version` prints the version;
any other argument is a usage error (exit 2), so a mistyped flag never starts a server
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
can run in a store already at `SMON_MAX_DB_BYTES`. The cap is one conditional `INSERT … WHERE COUNT(*) < cap`, so
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
the limit ago, it skips that round and lets the coordinator make its first pass. Supporting tasks stay *degraded*, with
no exit. The systemd unit backs restarts off from 15 s to 10 minutes (`RestartSteps`,
systemd ≥ 254) under a start limit of 10 in 30 minutes that a backed-off loop never
reaches: a slow dependency (a late volume) is retried until it appears, and a
deterministic crash is spaced out instead of looping every few seconds. Older systemd
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
