# Service Monitor

Tell the truth about availability — including when the monitor itself cannot see.

A Rust engine that probes a small number of authorized HTTP services on a schedule,
stores evidence in local SQLite, alerts through signed webhooks, and reports through a
CLI and a static SvelteKit dashboard — and never paints missing data green. `unknown`
is a first-class status; `coverage` (did the slots run?) and `success` (did what ran
pass?) are separate numbers everywhere.

## Status

**M0–M5 built and verified.** Every milestone landed with a live drill or a measured
run; what exists is what the commits show. Highlights:

- **Observing loop:** one attempt per scheduled slot, bounded global + per-origin
  concurrency, counted `missed` slots, generation invalidation (edits kill stale
  in-flight probes), restart rebuild from committed evidence.
- **Honest classification:** pass / http_fail / transport / timeout / missed — a
  redirect, a refused socket and a hung target never collapse into each other.
- **Incidents + alerts:** three failures open, two fresh passes recover; signed
  webhooks (`x-smon-event`, HMAC-SHA256 `x-smon-signature`) with finite jittered
  retries — the outbox survives restarts and restores.
- **SSRF guard:** `http`/`https` only; private/loopback/link-local/multicast/v4-mapped
  IP literals refused; DNS answers validated inside the resolver (a public name
  resolving to a private address is refused — `rejected_local`); redirects followed
  ≤5 hops with every hop revalidated. `SMON_PROFILE=lab` permits local targets for
  fixtures.
- **Operate:** retention rolls expired checks into daily aggregates (open incidents
  keep raw evidence); `SMON_MAX_DB_BYTES` caps the store and `/readyz` goes 503 when
  the write path fails; `SMON_LOG_FILE` bounds logs to two generations; `VACUUM INTO`
  backups behind `POST /api/backup`; versioned releases + rollback by repoint.
- **Auth:** `SMON_ADMIN_TOKEN` → `Bearer` for CLI/scripts, or the HttpOnly
  `smon_session` cookie the dashboard earns at `POST /api/login` (store-side
  expiry, `SMON_SESSION_TTL_S`; `Origin` must match `Host`, mandatory on
  cookie-authed writes). Tokenless loopback is dev mode, tokenless public bind
  refuses to boot.
- **Public status:** self-expiring `status.json` heartbeat + static renderer +
  independent checker — an expired snapshot reads "monitoring unavailable," never
  "all down."
- **Evidence:** 90+ Rust tests, a Playwright suite against the real server, a measured
  load baseline (`perf/results/`, 4000 checks, zero missed, p95 commit lag 4.9 s) —
  and the guide's 24 h soak is documented as *not yet run* rather than claimed.

## Layout

| Path | What it is |
|---|---|
| `crates/monitor-domain` | Pure domain: types, validation, the incident state machine. No I/O — enforced by `scripts/check-architecture.sh`. |
| `crates/monitor-db` | SQLite persistence (`sqlx`, WAL + foreign keys) and the single-writer task. Migrations in `crates/monitor-db/migrations/`. |
| `crates/monitor-server` | The `monitor-server` binary: Axum management API, coordinator + probe workers, alert deliverer, snapshot writer. |
| `crates/monitor-cli` | The `monitor` binary: talks to the HTTP API, same path as the dashboard. |
| `apps/dashboard` | SvelteKit static app (Tailwind v4, shadcn-svelte). Vite dev on :58600 proxies the API to :58400. |
| `apps/status` | Static status-page renderer for the public snapshot. |
| `docs/END_GOAL.md` | What done means; in/out of scope; the ship gate. |
| `docs/ARCHITECTURE.md` | Sources of truth, process model, data paths, bounds. |
| `docs/ROADMAP.md` | Milestones M0–M5; every step's outcome/invariant/failure case/acceptance test. |
| `docs/modules/` | Per-module contracts with testing parameters (01 domain … 09 testing/load). |
| `docs/DECISIONS.md` | Permanent `D-NNN` decisions, newest first. |
| `fixtures/`, `perf/` | Deterministic fault fixtures, the lab-profile load harness, committed results. |

## Quickstart

```bash
# Toolchain: Homebrew's cargo ignores rust-toolchain.toml — use rustup's proxies.
export PATH="$(brew --prefix rustup)/bin:$PATH"

npm install                 # once
cargo build                 # once (generates Cargo.lock)
scripts/dev.sh              # serves the API on 127.0.0.1:58400 (+ dashboard if built)

# in another shell
monitor() { cargo run -q -p monitor-cli -- "$@"; }
monitor ping
monitor monitors add example https://example.com/health
monitor monitors list

# dashboard dev server (hot reload, proxies /api to 58400)
npm run dev --workspace @service-monitor/dashboard   # http://127.0.0.1:58600

# or build it once and let monitor-server serve it at 127.0.0.1:58400
npm run build --workspace @service-monitor/dashboard
```

## Install

```bash
export PATH="$(brew --prefix rustup)/bin:$PATH"
cargo build --release -p monitor-server -p monitor-cli
npm run build --workspace @service-monitor/dashboard
scripts/deploy.sh --prefix /opt/smon        # releases/<sha>/bin + current symlink
# edit /opt/smon/env, then install deploy/monitor-server.service (systemd) or
# deploy/com.shirase.smon.plist (launchd); see deploy/RUNBOOK.md
```

Upgrade = rerun `deploy.sh` + restart. Rollback = repoint `current` + restart —
data lives in `$PREFIX/data`, outside releases, so evidence survives both.
`deploy/RUNBOOK.md` covers install/upgrade/rollback/backup/heartbeat-watching end
to end. Supported platforms: anything Rust + Node build on; the unit files target
systemd (Linux) and launchd (macOS). v1 alerts are webhooks and SMTP email through the
same outbox — no PagerDuty.

## Configuration

All via `SMON_*` env vars (defaults in `deploy.sh`'s generated `env` file):

`SMON_DATABASE_URL` `SMON_LISTEN` `SMON_DASHBOARD_DIR` `SMON_ADMIN_TOKEN`
`SMON_PROFILE` (`lab` permits local targets — never in production) `SMON_MAX_DB_BYTES`
`SMON_RETENTION_S` `SMON_RETENTION_INTERVAL_S` `SMON_STATUS_DIR`
`SMON_STATUS_INTERVAL_S` `SMON_STATUS_TTL_S` `SMON_LOG_FILE` `SMON_LOG_MAX_BYTES`
`SMON_WEBHOOK_URL` `SMON_WEBHOOK_SECRET`
`SMON_SMTP_DSN` `SMON_SMTP_FROM` `SMON_SMTP_TO` — all three required together for the
email channel; a partial set fails the boot.
`SMON_SESSION_TTL_S` — dashboard session lifetime in seconds (default 43200).

## Verify

`scripts/check.sh` — toolchain check, `cargo fmt --check`, `cargo clippy -- -D warnings`,
`cargo test`, the architecture rule, `svelte-check` and a dashboard build. Plus the
drills: `scripts/e2e.sh` (Playwright), `scripts/backup-drill.sh` (backup → destroy →
restore), `perf/lab-profile.mjs` (load baseline).
