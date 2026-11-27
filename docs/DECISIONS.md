# Decisions

Newest first. `D-NNN` ids are permanent; referenced from code and build-log entries.

## D-023 — Any client keep-alive lives strictly below the server's header-read bound

`Limits::header_read_timeout` (10 s) is hyper's one knob and bounds two waits: the
time a request's headers may take AND the idle wait between requests on a
kept-alive connection. At the deadline hyper closes the connection with no
response — a clean FIN, but silent. A client or proxy whose own idle timeout is
longer will reuse a connection the server is about to close: Go/k6 reads
`readLoopPeekFailLocked` (`status 0`), and Caddy answers a non-idempotent request
with a 502 — a transport failure indistinguishable from a real outage, counted by
the load gate and served to a real caller.

The rule: the peer side expires first. Every upstream that holds connections to
smon keeps its idle bound strictly below 10 s, or never reuses at all — the server
bound stays a property of `Limits`, not of any client. In the deploy edge the
`transport http { keepalive 5s }` block expires Caddy's idle upstreams at half the
bound (the default 2 m would race every time). k6 cannot express an idle-timeout
below it (grafana/k6#3466 is still open), so the load scenarios set
`noConnectionReuse: true` — a fresh connection per request, which never races and
exercises the accept path anyway. `tests/serve.rs` pins the server's side of the
contract: idled past the bound, the connection ends in a clean close before any
bytes of a reuse — never a reset mid-request — and a fresh connection is served
whole.

## D-022 — Plan tiers live on the workspace row; limits enforce at write time

`workspaces.plan` is the closed set `free`/`operator` (migration 0015). Signup mints
`free`; the seeded default workspace is `operator`, so migrating never downgrades
the maintainer's own monitoring. A plan is a `monitor_domain::Plan` resolved
server-side on every write that could cross a limit: monitor count and interval
floor (`409 limit_reached` / `plan_limit`), evidence horizons (a plan-fixed
`RetentionPolicy`), while `operator` defers to the server's configured ceilings
(`SMON_MAX_MONITORS`, the 15 s domain floor, `SMON_RETENTION_*`). Retention reads
each workspace's resolved cutoffs per pass — a `free` workspace's rows expire on
the free clocks while an `operator` workspace's same-age rows are kept — and every
delete stays bounded per batch (D-012), sessions included. Only the operator moves
a workspace between plans (`PATCH /api/workspaces/{id}/plan`); a downgrade changes
what the next write may do, never pauses or deletes what already runs. Every
number on the free tier is provisional until droplet measurements exist.
Closes SAAS_PLAN open question 4.

## D-021 — Public status pages live at `/s/<workspace-slug>` on the same origin

One page per workspace, served from the monitor's own origin — no wildcard DNS, no
per-workspace certificate, nothing new for the Caddyfile to know. Subdomains stay
possible later if asked for; nothing about `/s/` forecloses them. The slug is
workspace data and therefore workspace-scoped like everything else (D-020).
Closes SAAS_PLAN open question 2.

## D-020 — Accounts, workspaces, members and tokens live in the same SQLite store

SAAS_PLAN open question 1 — Neon for account records — is closed: **no second
database**. Accounts, workspaces, members and API tokens join monitors, checks,
incidents and the outbox in the one SQLite store the product already has. The
engine never depends on a network database (a Neon outage can no longer stop
anything), the product stays self-contained and self-hostable as a single
binary plus a volume, and the existing backup endpoint and restore drill cover
account records with the same evidence path — nothing new to back up, encrypt or
restore. The cost carried on the record: no managed, multi-AZ database for
account data; the volume snapshot story (`smon-backup.sh` → age → R2) is the
whole story, and it is already drilled. Every tenant-owned row carries
`workspace_id`; the store API takes a `WorkspaceId` so an unscoped query cannot
compile.

## D-019 — Backups are confined to `SMON_BACKUP_DIR` under server-drawn names

`POST /api/backup` took an absolute `path` — a write-anywhere primitive the admin token's
leak would turn into arbitrary file creation as the service user, and a footgun for
clobbering files the store didn't write. Now the endpoint takes only `{}`: the snapshot
lands in `SMON_BACKUP_DIR` (absolute, created owner-only at boot so a bad path fails the
boot) as `monitor-<UTC>-<random>.db` through `create_new`+0600, and a body that names a
path — or anything else — is refused with 422. Unset dir → 404 `backup_disabled`. The
response returns `{"name","bytes"}`; the CLI (`monitor backup`) and the dashboard's "Back
up now" both go through it, so no path is a back door the API has but the clients can't use.

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
  A write-time origin is the **resolved address and port plus the requested host**
  (`origin.rs` key `addr:port|host`): spelling variants of one host collapse (case,
  trailing dot), but `localhost:9` and `127.0.0.1:9` are distinct origins — shared
  hosting puts thousands of sites on one address. The workspace's own ceiling is
  judged on the **resolved address alone** summed across hosts (the write refuses
  once the caller's share of the IP:port is spent), and the cross-tenant runtime
  ceiling lives where the connection's target is first known — inside the SSRF
  resolver's per-address starts budget (misses counted). Names are resolved through
  a process-wide cache (5 min; the coordinator refreshes every monitor's name in
  the background, a write resolves its own within 3 s); a name not resolved yet
  keys by its spelling.
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

It becomes one the moment someone other than the operator can set a webhook URL — and it
became one at S1d, when workspaces gained their own channels (`workspaces.webhook_url` /
`webhook_secret`, the `tenant_webhook` outbox channel, `PUT /api/workspace/webhook`).
The gate is built exactly as the paragraph above demanded:

- **At save:** `ProbePolicy::check_resolved` — the scheme/literal check, then a bounded
  lookup where every returned address must be public. A name that cannot be resolved is
  refused (`invalid_request`); an unproven host is not a proven-public host.
- **At every send:** the deliverer re-parses the row's URL, re-runs `check` for scheme
  and literal (reqwest never consults DNS for an IP literal, so the literal gate is the
  only thing between one and a socket), and lets the strict client re-resolve through
  `PolicyResolver` — validation *is* the resolution, so there is no check/connect TOCTOU
  and a name rebound to a private address is refused inside the connect.
- **The strict client:** no proxy, no redirects (a 3xx is a failed `http_3xx` attempt,
  never a followed hop), the shared per-attempt `attempt_deadline`, and the response
  body is never read — the size cap is literal zero bytes.
- **Refusals are evidence:** a send-time refusal is committed on the row as
  `notify_outbox.last_error`, the probe taxonomy's stable words (`rejected_local`,
  `rejected_scheme`, …) — never a URL, header or error string. Success clears it.

**Scope: this guard applies to tenant URLs only.** The operator's own global channels
(`webhook` from `SMON_WEBHOOK_URL`, `email` from `SMON_SMTP_*`) keep today's behaviour —
operator configuration is not a privilege boundary. Pinned by
`tenant_send_refuses_every_blocked_literal`,
`a_rebound_name_is_refused_inside_the_connect_and_recorded`,
`a_redirect_to_a_private_address_is_never_followed` (deliver.rs) and the save-gate
matrix in `tests/webhooks.rs`.

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
`committing_a_backlog_of_misses_keeps_the_heartbeat_fresh`; the real-process drill is now
scripted as `scripts/freeze-drill.sh` — it freezes the server ~135 s and asserts no exit,
the missed rows, and the honest recovery — 130 s SIGSTOP drill not re-run yet.) Supporting tasks stay *degraded*, with
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

Scope (amended 2026-11-19): this covers structure and conventions only — never the visual
system. Per the maintainer, Colophon's look is Colophon's: Service Monitor must not use its
palette, its Geist Pixel + Inter pairing, its panels or its motifs. Smon's own design system
is chosen later and replaces `apps/dashboard/src/lib/components/ui/styles.css`, which holds
neutral placeholder tokens until then.

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

## D-024 — The per-address probe budget is a global ceiling with a per-workspace share

*The round-robin queue described below was removed by D-028; the ceiling, the share
and where the charge is made still stand.*

Every probe start pays one resolved IP's token bucket — 8/s shared across all
workspaces (the ceiling the write-time `addr:port` bound assumes), under a 2/s
per-workspace share so the heaviest spender stops at its own rate. While the
ceiling has room the static share is the whole rule; once it is spent the
ceiling becomes a **round-robin queue**, not arithmetic: every workspace
whose start waits on the ceiling holds its place in a bounded FIFO per
address (128 waiters), and each global token goes to the head in arrival
order — first-come can never starve a newcomer, because every waiting
workspace reaches the head within ⌈waiters ÷ ceiling⌉ seconds while it keeps
asking (the fifth re-review killed the max-min-split arithmetic: under it a
hard cap of 8 active workspaces made every newcomer's split < 1 start, so
twenty paced tenants starved twelve of themselves and eight greedy tenants
starved their ninth). A claim costs nothing while it waits — no tokens are
spent — and it leaves the queue when its turn arrives, when its own share
cannot pay, or when it stops asking for five minutes (a >5-minute-interval
workspace under contention that long can lose its place; it re-queues, never
silences). Total admits stay a pure token stream — burst + rate·t over any
window — because a queued claim pays its global token only when served. The charge lives inside the SSRF validating resolver on the
ONE candidate offered to the dialer — the first paying address in answer
order; a fallback family or a sprayed answer's tail is never dialed and never
pays. The honest trade-off of offering one: hyper collects the offered
addresses eagerly and reqwest exposes no dial hook, so a multi-candidate
answer could only be charged upfront — paying for addresses never dialed
(the N-2 hole) — and there is no Happy-Eyeballs fallback: a dual-stack
target whose first answer is a dead family records a transport failure
instead of falling back. That misclassification can page, and it is the
chosen price of never charging the victim for connections that never happen.
Beside the resolver, for the paths that never touch DNS: an IP-literal URL
pays its own literal, and the redirect closure pays each hop's literal host
before following it, refusing the hop as `address_budget`. IPv4-mapped IPv6 spellings normalize to
the IPv4 key — `::ffff:8.8.8.8` and `8.8.8.8` share one bucket. The bucket map
is bounded (8192 entries); when it fills it evicts only buckets that have
fully refilled, and when none qualify it refuses the *new* address as a
counted miss — it never clears, so a spent bucket stays spent under churn. A
refusal commits `missed` + `address_budget`: a coverage gap that reports
`unknown` but is neutral on the monitor's failure/recovery streaks — in memory
AND in the SQL (`first_fail_at` never sees it as a boundary, the rebuild tail
windows past it), so the outage evidence a restart replays matches what the
live machine held. The webhook/test-alert channel is deliberately NOT on this
budget: sends are member- or incident-initiated, outbox- and retry-bounded —
a different bound for a different channel, and charging them here would let a
busy webhook URL starve unrelated probes at the same address (explicit gap,
N-6).

## D-025 — A response-body assertion miss is `http_fail` evidence, never a new outcome

A monitor can carry a bounded response contract — a status set (≤8 inclusive
ranges over 1–599) and an optional literal body marker (1–256 wire bytes,
searched only in the first 64 KiB actually received inside the probe deadline;
the client never decompresses, so compressed bytes are searched exactly as they
arrive and no compressed bomb can inflate in-process). When the status is
acceptable but the marker is absent from that bounded prefix the check commits
`http_fail` with `detail = 'assert_body'` and the observed status — the target
*answered correctly* and the *observed evidence* contradicts the contract, so
the failure is HTTP-visible, streak-counting failure evidence — a real miss,
not a coverage gap. The alternative, a new `CheckOutcome` variant, was
rejected: it would need a decode migration for stored `outcome` strings,
dashboard/outbox/aggregation vocabulary churn, and a semantic claim ("the
assertion failed") that the wire already encodes — status acceptable, content
wrong. `assert_body` joins the detail vocabulary beside `address_budget` and
the guide-drift pins cover it in both directions. Two honest edges: a marker
that lands past the 64 KiB cap misses — the prefix is all the evidence there
is — and HEAD can never pair with a body marker (refused at write, create and
edit alike, because a HEAD probe reads nothing to assert on). Method is opt-in
`GET`/`HEAD`; HEAD is preserved across every redirect including 303 and a 405
or 501 answer is committed as that status — never rewritten to GET or retried.

## D-026 — A monitor can be deleted with its evidence, only by saying so

`DELETE /api/monitors/{id}` keeps its old answer: a monitor that has recorded a
check or an incident is refused with 409, so no client removes history by
accident. `DELETE /api/monitors/{id}?evidence=delete` is the explicit form: it
removes the monitor's queued alerts, incidents, checks and daily aggregates and
then the monitor, whose maintenance windows and status-page listing cascade.
Any other value of `evidence` is 422. Member work, like every other monitor
write, and it spends the same per-workspace write budget. The dashboard always
asks through a confirmation that names what is removed.

It is not one transaction, on purpose. The store has one write lock and every
workspace's observation commits wait on it, so the delete first pauses the
monitor and bumps its generation, then removes the evidence in batches of 2,000
rows, each its own short transaction, and only the last step (whatever arrived
meanwhile, plus the monitor row) is atomic. A failure part-way leaves a paused
monitor with some evidence gone; asking again finishes it. The child-side
indexes this needs (outbox by incident and by monitor, incidents by monitor,
status listing by monitor) came with it: without them SQLite scanned a whole
child table for every parent row deleted.

Why the earlier rule (the gap log's "history is preserved by not deleting") had
to bend for the hosted product: a workspace's evidence is the workspace's own
data, the plan caps monitors and paused ones count, and a monitor registered
with a typo recorded a check within one interval and then occupied a slot for
ever. The hosted plan also names "delete workspace (and its evidence)" as a
requirement; this is the same right at the size of one monitor.

What is given up, stated plainly: after such a delete the incidents and checks
of that monitor are gone and no export can bring them back, an open incident's
pending alerts are dropped with it (no "recovered" alert follows), and the
aggregates no longer show that the monitor existed. A probe in flight at that
moment is dropped: as stale once the generation is bumped, and, if its commit
lands after the monitor row is gone, as "monitor deleted" rather than as a
failed write (a failed write counts against the whole engine's health).
Every statement is bound to the caller's workspace: another workspace's monitor
id deletes nothing, and a test weakening that binding fails.

## D-027 — An account can change its password, delete itself, and take its data with it

Until now an account could sign up, sign in and sign out. It could not change its
password, could not leave, and a workspace could not take its record anywhere. The
hosted plan names "delete workspace (and its evidence), export evidence" as a
requirement; these three routes are that requirement.

**Who may ask.** `POST /api/account/password` and `DELETE /api/account` answer only an
account session. A workspace API token is the workspace's credential and the operator
has no account, so both get 403. Both routes ask for the account's password again, and
that check is a credential guess like a sign-in: it takes a guess from the caller's
address before anything is compared (429 once the address has none left), runs under
the same password-work limit (503 `busy` when the queue is full), answers a wrong
password with the sign-in's own 401, and gives the guess back when the password is
right. `GET /api/workspace/export` is owner work; the operator acts at owner level in
the workspace it picks, and a token never holds owner, so every token gets 403.

**Changing a password.** The new password follows the sign-up rule (8 to 256 bytes),
checked by the same function. One transaction stores the new hash and ends every other
session of the account; the session that made the change stays signed in. Sign-in now
creates its session only while the hash it verified is still the stored one, so a
sign-in that was verifying the old password while the change committed gets a 401, not
a session that outlives the change.

**Deleting an account — what is deleted.** In one transaction:

- every workspace whose only member is the account, with everything recorded in it:
  monitors, checks, daily aggregates, incidents, queued alerts, maintenance windows,
  the status page's listing and the slugs it held, API tokens, pending invitations and
  the member row;
- the account's seat in every other workspace;
- invitations addressed to its email;
- all its sessions, and the account row.

After the commit the schedule is republished without the deleted monitors, the deleted
workspaces' status pages stop resolving at once and their snapshot files are removed,
and the response expires the session cookie.

**What is refused.** While the account is the only owner of a workspace that has other
members the answer is 409: `the workspace "<name>" needs an owner: make another member
an owner, then delete your account`. The same rule as demoting or removing the last
owner, for the same reason: the others would be left in a workspace nobody can
administer. Nothing is deleted and the account stays signed in.

**What is kept on purpose.**

- The seeded default workspace is never deleted, whoever sits in it: it is the
  operator's. An account that is its only member just leaves it.
- A shared workspace keeps the API tokens the leaver minted. They are the workspace's
  credentials and its remaining owners can revoke them; only "created by" is cleared.
- The signup invite the account used stays spent.
- `accounts.workspace_id` (where a session lands when it names no workspace) has no
  cascade. If a deleted workspace is still the home of some *other* account — one that
  left it or was removed from it — that account is moved to the workspace it joined
  first, or to the seeded default workspace when it has no seat anywhere. It gets no
  seat it did not have: an account with no membership answers `not_a_member` before
  and after.

**What is given up, stated plainly.**

- There is no undo and no grace period. The checks, incidents and aggregates of a
  deleted workspace are gone, and only an export taken beforehand has them.
- Open incidents' pending alerts are dropped with the workspace: no "recovered" alert
  follows. A probe in flight at that moment commits against a monitor that is gone,
  which the writer already drops.
- The workspace's status-page slug, and the slugs it held, are free again. A later
  workspace can publish under a name readers knew. Holding names for ever on behalf
  of workspaces that no longer exist was judged the worse trade.
- The delete is one write transaction on a single-writer store, sized by the
  workspace's plan (tens of thousands of rows on the free plan). Probe commits wait
  behind it for that long. The deletes reach checks and aggregates through the
  per-monitor indexes, never by scanning other tenants' rows.
- Backups taken before the deletion still hold the account and its data until they
  age out.

**The export.** One JSON document for the caller's workspace: the workspace (id, name,
slug, plan), the roster as email and role, the monitors as the API shows them,
maintenance windows, incidents, daily aggregates and raw checks. It never carries the
webhook URL or its signing secret, API tokens or their digests, password hashes,
sessions or invitations, and every read goes through the workspace's own view of the
store. It is bounded: each monitor's newest 5,000 checks and at most 50,000 checks in
all (`checks[].truncated` says when a monitor has more), the newest 5,000 incidents
and 5,000 maintenance windows and 40,000 daily rows (the top-level `truncated` object
says when there are more), and one export per workspace every 10 seconds (429 with
`Retry-After` inside that). It is sent as an attachment named
`service-monitor-export-<slug>.json` and marked `no-store`.

Pinned by `tests/account.rs` in both crates: the store tests count every tenant-owned
table through a second connection before and after a deletion, and the API tests
assert each refusal by its sentence and re-read the stored state afterwards.

## D-028 — The per-address budget is two token buckets and nothing else (replaces D-024's queue)

D-024's ceiling and share stay: 8 probe starts a second at one resolved address
across every workspace, and 2 a second for each workspace there. What goes is
the queue that took over once the ceiling was spent. A start is now paid when
the address's bucket and the workspace's own bucket each hold a token, and
refused otherwise. A refusal remembers nothing.

Why the queue went. The scheduler asks once per slot and never comes back for
an answer, so a "place in line" was only the memory of a refused call, and only
the head of the line could be paid. A head that was not asking at that moment
blocked the address until it had been silent for five minutes, with tokens in
the bucket the whole time. Two workspaces with two monitors each on one address
were enough: after one burst emptied the bucket once, half their monitors were
never served again. Each refusal is recorded as `missed` with `address_budget`,
which is neutral to streaks, so a real outage on those monitors would not have
opened an incident. A test now runs that schedule for ten minutes and requires
every start to be paid.

What this rule guarantees: below the ceiling every start is paid; no workspace
takes more than its share at an address, so three workspaces at their full share
still leave room for a fourth; total starts at an address never exceed
`burst + rate·t`. What it does not guarantee: an order. If workspaces together
ask for more than 8 starts a second at one address for a sustained time, the
tokens go to whoever asks when one is there, and the rest are recorded as
missed with the reason. Reaching that takes four workspaces each running their
full 2 starts a second at one address, which the free plan's limits (10
monitors, one check a minute) cannot do: it needs operator-plan workspaces or
about fifty free ones aimed at the same address. The coverage figure shows it
when it happens. A bounded-wait guarantee would need refused starts to wait for
a token, with a claim per monitor released on cancel; that is a scheduler
change to make when real load asks for it, not a queue beside a caller that
does not wait.

Also fixed here: a bucket's clock never moves backwards (the clock is read
before the lock, and a reversed arrival used to credit the gap twice), and an
address forgets workspace shares that have fully refilled once it holds 256.

## D-029 — A probe's source address is not a credential, and body text is checked wherever the redirect ends

A monitor may follow redirects to another public host, and a body expectation
is checked against the final response. A workspace could therefore register a
URL of its own that redirects to someone else's public URL and learn, one bit
per check, whether the first 64 KiB of that response contains a string. The
same is already true of the status code. The bound is unchanged: only public
addresses are ever dialed, every hop is revalidated, and the request carries no
credential of anyone's. The remaining exposure is a third party that serves
different content to this service's egress address because it trusts that
address, and such trust is the third party's mistake to make: the egress
addresses of a public monitoring service are published facts, not secrets.
Restricting the check to the registered origin was rejected because ordinary
sites redirect between origins (to `https`, to `www`), and those are exactly
the pages a body expectation is for.
