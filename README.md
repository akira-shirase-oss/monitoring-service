# Service Monitor

Tell the truth about availability — including when the monitor itself cannot see.

The deliberate rebuild described in the Akira Shirase OSS project guides
(`Project_Guides/02_Service_Monitor_Rebuild.docx`): a Rust engine that probes a small number of
authorized HTTP services on a schedule, stores evidence in local SQLite, reports through a CLI
and a static SvelteKit dashboard, and never paints missing data green.

## Status

**Scaffold.** What exists today:

- Workspace, toolchain pin (Rust 1.99.1), lint/architecture gates and check scripts.
- `monitor-domain`: monitor types, the check-outcome model and the incident state machine
  (three failures open, two fresh passes recover, missed slots reset streaks to `unknown`,
  incidents survive restarts) with unit tests.
- `monitor-db`: SQLite store (WAL, foreign keys) with migration `0001` — monitors, checks,
  incidents, notify outbox.
- `monitor-server`: `/livez`, `/readyz`, `GET|POST /api/monitors`, `GET /api/monitors/{id}`;
  serves `apps/dashboard/build` on the same origin when built. No probing yet.
- `monitor` CLI: `ping`, `monitors list`, `monitors add`.
- `apps/dashboard`: static SvelteKit + Tailwind v4 + shadcn-svelte; an unreachable server is
  shown as "cannot see", not as a green board.

Built: coordinator (one attempt per slot, bounded concurrency, counted misses), the probe
engine (GET only, deadline + body bounds, classified outcomes, URL-layer target policy), the
single-writer task (check + incident + outbox in one transaction), restart rebuild, and real
statuses in the API. `SMON_PROFILE=lab` permits loopback targets for fixtures.

M2 "notify" landed: the outbox deliverer posts signed webhook events (`x-smon-event` +
HMAC-SHA256 `x-smon-signature`, finite jittered retries, exhausted rows stay visible;
`SMON_WEBHOOK_URL`/`SMON_WEBHOOK_SECRET`), `PATCH`/`DELETE /api/monitors/{id}` bump
`generation` atomically so late probes die as stale (deletes refuse once evidence exists),
`GET /api/monitors/{id}/checks` + `GET /api/incidents` feed the dashboard detail page, the
CLI grew `show/edit/pause/unpause/delete`, and `fixtures/fault-server.mjs` provides
ok/fail/slow/hang/status/flaky targets plus a `/sink` webhook receiver.

Not yet: the public status renderer, auth, redirect/DNS-level SSRF guards, retention and
load runs. Numbers in docs are targets, not measurements.

## Layout

| Path | What it is |
|---|---|
| `crates/monitor-domain` | Pure domain: types, validation, the incident state machine. No I/O — enforced by `scripts/check-architecture.sh`. |
| `crates/monitor-db` | SQLite persistence (`sqlx`, WAL + foreign keys) and the single-writer task. Migrations in `crates/monitor-db/migrations/`. |
| `crates/monitor-server` | The `monitor-server` binary: Axum management API, coordinator + probe workers, alert deliverer. |
| `crates/monitor-cli` | The `monitor` binary: talks to the HTTP API, same path as the dashboard. |
| `apps/dashboard` | SvelteKit static app (Tailwind v4, shadcn-svelte). Vite dev on :58600 proxies the API to :58400. |
| `docs/END_GOAL.md` | What done means; in/out of scope; the ship gate. |
| `docs/ARCHITECTURE.md` | Sources of truth, process model, data paths, bounds. |
| `docs/ROADMAP.md` | Milestones M0–M5; every step's outcome/invariant/failure case/acceptance test. |
| `docs/modules/` | Per-module contracts with testing parameters (01 domain … 09 testing/load). |
| `docs/DECISIONS.md` | Permanent `D-NNN` decisions, newest first. |
| `fixtures/`, `perf/` | Failure fixtures and load runs — placeholders until the coordinator exists. |

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

## Verify

`scripts/check.sh` — toolchain check, `cargo fmt --check`, `cargo clippy -- -D warnings`,
`cargo test`, the architecture rule, `svelte-check` and a dashboard build.
