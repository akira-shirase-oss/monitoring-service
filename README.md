# Service Monitor

Tell the truth about availability — including when the monitor itself cannot see.

A Rust engine that probes a small number of authorized HTTP services on a schedule,
stores evidence in local SQLite, alerts through signed webhooks, and reports through a
CLI and a static SvelteKit dashboard — and never paints missing data green. `unknown`
is a first-class status; `coverage` (did the slots run?) and `success` (did what ran
pass?) are separate numbers everywhere.

## Status

**M0–M7 built and verified.** Every milestone landed with a live drill or a measured
run; what exists is what the commits show. Highlights:

- **Observing loop:** one attempt per scheduled slot, bounded global + per-origin
  concurrency, counted `missed` slots, generation invalidation (edits kill stale
  in-flight probes), restart rebuild from committed evidence.
- **Honest classification:** pass / http_fail / transport / timeout / missed — a
  redirect, a refused socket and a hung target never collapse into each other; TLS
  failures classify deterministically (`cert_expired`, `cert_name_mismatch`, …) on
  webpki-roots rather than the platform verifier.
- **Incidents + alerts:** per-monitor thresholds (1–10 to open / to recover, default
  3/2) and all four honest times (`first_fail_at`, `last_fail_at`, `opened_at`,
  `resolved_at`); maintenance windows suppress the page, never the evidence. Signed
  webhooks (`x-smon-event`, HMAC-SHA256 `x-smon-signature`) and SMTP email through
  the same outbox, with finite jittered retries — the outbox survives restarts and
  restores.
- **SSRF guard:** `http`/`https` only; every non-global IPv4/IPv6 special-purpose
  block refused (private, loopback, link-local, CGNAT, benchmarking, documentation,
  reserved, multicast, and v4-mapped/NAT64/6to4 forms of them); DNS answers validated inside the resolver (a public name
  resolving to a private address is refused — `rejected_local`); redirects followed
  ≤5 hops with every hop revalidated. `SMON_PROFILE=lab` permits local targets for
  fixtures.
- **Operate:** retention rolls expired checks into daily aggregates and gives every
  table a horizon (D-012 — an open incident keeps its birth streak raw; delivered
  alerts 30 d, resolved incidents and aggregates 365 d; `SMON_MAX_MONITORS` caps
  monitors at 100); `SMON_MAX_DB_BYTES` caps the store and `/readyz` goes 503 when
  the write path fails; `SMON_LOG_FILE` bounds logs to two generations; `VACUUM INTO`
  backups behind `POST /api/backup`; versioned releases + rollback by repoint.
- **Auth:** `SMON_ADMIN_TOKEN` (at least 32 bytes, no spaces or `#` — e.g.
  `openssl rand -hex 32`; a weaker value refuses to boot) → `Bearer` for CLI/scripts, or the HttpOnly
  `smon_session` cookie the dashboard earns at `POST /api/login` (store-side
  expiry, `SMON_SESSION_TTL_S`; `Origin` must match `Host`, mandatory on
  cookie-authed writes). Sessions are stored as token-bound digests and end when
  the token rotates or after `SMON_SESSION_IDLE_S` unused (D-016). Every `/api` route and method is proven to
  refuse a missing credential, failed credentials are throttled per client address
  (`SMON_TRUSTED_PROXY` behind a proxy), and bodies are capped at 16 KiB with a 10 s deadline.
  Tokenless loopback is dev mode — it serves only loopback `Host` values and same-origin
  browsers — and a tokenless public bind refuses to boot. Store failures answer fixed
  text; every response carries `x-request-id`.
- **Dashboard:** add/edit/pause/delete without leaving the UI, an uptime strip that
  renders unobserved slots as gaps (never interpolated green), a 15 s auto-refresh
  with atomic swaps, the public status rendered in-app, and a guide page whose
  words are drift-checked against the real wire vocabulary in both directions.
  Every response carries a CSP (scripts from this origin plus the build's inline
  bootstrap by hash, no framing), `nosniff` and `Referrer-Policy: same-origin`.
- **Public status:** self-expiring `status.json` heartbeat + static renderer +
  independent checker — an expired snapshot reads "monitoring unavailable," never
  "all down."
- **Evidence:** 275 Rust tests run by hosted CI on every push (`.github/workflows/ci.yml`,
  pinned actions, read-only token, daily advisory sweep; Dependabot weekly), a
  Playwright suite against the real server, a
  mutation-tested domain (48 mutants → 42 caught, 0 missed), pinned CLI exit codes,
  a measured load baseline and a 3.84M-request real-app drill (`perf/results/`,
  0 missed slots, honest incident times, both alert channels delivered) — and the
  guide's 24 h soak is still documented as *not yet run* rather than claimed.

## Layout

| Path | What it is |
|---|---|
| `crates/monitor-domain` | Pure domain: types, validation, the incident state machine. No I/O — enforced by `scripts/check-architecture.sh`. |
| `crates/monitor-db` | SQLite persistence (`sqlx`, WAL + foreign keys) and the single-writer task. Migrations in `crates/monitor-db/migrations/`. |
| `crates/monitor-server` | The `monitor-server` binary: Axum management API, coordinator + probe workers, alert deliverer, snapshot writer. |
| `crates/monitor-cli` | The `monitor` binary: talks to the HTTP API, same path as the dashboard. |
| `apps/dashboard` | SvelteKit static app (Tailwind v4, shadcn-svelte). Vite dev on :58600 proxies the API to `scripts/dev.sh` on :58465. |
| `apps/status` | Static status-page renderer for the public snapshot. |
| `docs/END_GOAL.md` | What done means; in/out of scope; the ship gate. |
| `docs/ARCHITECTURE.md` | Sources of truth, process model, data paths, bounds. |
| `docs/ROADMAP.md` | Milestones M0–M7; every step's outcome/invariant/failure case/acceptance test. |
| `docs/modules/` | Per-module contracts with testing parameters (01 domain … 09 testing/load). |
| `docs/DECISIONS.md` | Permanent `D-NNN` decisions, newest first. |
| `fixtures/`, `perf/` | Deterministic fault fixtures, the lab-profile load harness, committed results. |

## Quickstart

```bash
# Toolchain: Homebrew's cargo ignores rust-toolchain.toml — use rustup's proxies.
export PATH="$(brew --prefix rustup)/bin:$PATH"

npm install                 # once
cargo build                 # once (generates Cargo.lock)
scripts/dev.sh              # serves the API on 127.0.0.1:58465 (+ dashboard if built);
                            # SMON_DEV_PORT overrides; never 58400 (an installed monitor's)

# in another shell — point the CLI at the dev server, not an installed monitor on :58400
monitor() { SMON_SERVER=http://127.0.0.1:58465 cargo run -q -p monitor-cli -- "$@"; }
monitor ping
monitor monitors add example https://example.com/health
monitor monitors list

# dashboard dev server (hot reload, proxies /api to the dev server on 58465)
npm run dev --workspace @service-monitor/dashboard   # http://127.0.0.1:58600

# or build it once and let the dev server serve it at 127.0.0.1:58465
npm run build --workspace @service-monitor/dashboard
```

## Install

```bash
export PATH="$(brew --prefix rustup)/bin:$PATH"
cargo build --release -p monitor-server -p monitor-cli
npm run build --workspace @service-monitor/dashboard
scripts/deploy.sh --prefix /opt/smon        # releases/<sha>/bin + current symlink
# a new install creates its store once: monitor-server --init (RUNBOOK, Install)
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

All via `SMON_*` env vars (defaults in `deploy.sh`'s generated `env` file). They are
parsed once at boot (`crates/monitor-server/src/config.rs`): an invalid or blank value
fails the boot with every problem listed — never a silent fall-back to the default (an
inline `# comment` after a value counts: systemd keeps it as part of the value) — and
unknown `SMON_*` names are logged as likely typos. Secrets and webhook URLs are never
echoed in those messages.

`SMON_DATABASE_URL` `SMON_LISTEN` `SMON_DASHBOARD_DIR` `SMON_ADMIN_TOKEN`
`SMON_PROFILE` (`lab` permits local targets — never in production) `SMON_MAX_DB_BYTES`
`SMON_RETENTION_S` `SMON_RETENTION_INTERVAL_S` `SMON_AGGREGATE_RETENTION_S`
`SMON_INCIDENT_RETENTION_S` `SMON_OUTBOX_RETENTION_S` `SMON_MAX_MONITORS` `SMON_STATUS_DIR`
`SMON_STATUS_INTERVAL_S` `SMON_STATUS_TTL_S` `SMON_LOG_FILE` `SMON_LOG_MAX_BYTES`
`SMON_WEBHOOK_URL` `SMON_WEBHOOK_SECRET`
`SMON_SMTP_DSN` `SMON_SMTP_FROM` `SMON_SMTP_TO` — all three required together for the
email channel; a partial set fails the boot.
`SMON_SESSION_TTL_S` — dashboard session lifetime in seconds (default 43200).
`SMON_SESSION_IDLE_S` — a session unused this long ends, in seconds (default 3600).
`SMON_MAX_CONNECTIONS` — HTTP connections served at once (default 1024; keep it well below
the file-descriptor limit). Past it new connections wait in the backlog. A request header
must arrive within 10 s, and an idle keep-alive connection is closed after 10 s.
`SMON_WRITE_STALL_MS` — a response write that makes no progress this long closes the
connection (default 10000), so a client that never reads can't hold a slot. A connection is
also closed after 1000 requests or 5 minutes, after its current response.
`SMON_TRUSTED_PROXY` — the reverse proxy's IP address. Failed credentials (login or bearer)
are throttled per client address — five back to back, then one every 2 s; a right token
costs nothing — and behind a proxy every client shares its address unless this names it
(then the right-most `X-Forwarded-For` entry is the client). Rotating
`SMON_ADMIN_TOKEN` ends every session at once (D-016).
`SMON_COOKIE_SECURE` — `auto` (default: the session cookie is `Secure` whenever the TLS
proxy in front reports `X-Forwarded-Proto: https` or `Forwarded: proto=https`) or
`always` (for a proxy that doesn't forward the scheme).

## Verify

`scripts/check.sh` — toolchain check, `scripts/check-ignored.sh` (nothing but known
build/runtime artifacts may be git-ignored), `cargo fmt --check`, `cargo clippy -- -D
warnings`, `cargo test`, `scripts/check-advisories.sh` (`cargo deny check advisories bans licenses sources`,
required when `CI` is set; each `deny.toml` exception must carry a re-check date that has
not passed), the architecture rule, `svelte-check`, a dashboard build and `npm audit
--omit=dev --audit-level=moderate`. CI runs the same script on every push and pull request
(ubuntu-latest). Plus the
drills: `scripts/e2e.sh` (Playwright), `scripts/backup-drill.sh` (backup → destroy →
restore), `perf/lab-profile.mjs` (load baseline). Every drill and harness runs on its own
ports (never 58400, an installed monitor's), refuses a busy one, and stops only what it
started.

## License

MIT — see [LICENSE](LICENSE). Copyright (c) 2026 akira-shirase-oss.
