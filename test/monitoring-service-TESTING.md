# Testing guide: Service Monitor (`monitoring-service`)

https://github.com/akira-shirase-oss/monitoring-service

Service Monitor is a Rust HTTP uptime monitor with a local SQLite store, a CLI, and a static SvelteKit dashboard. It probes a small number of authorized HTTP services, keeps evidence (including missed slots), and can alert through signed webhooks and SMTP. `unknown` is a first-class status; coverage (did the slots run?) and success (did what ran pass?) are separate numbers, and missing data is not supposed to be painted green.

Written against commit `fb3e966` (2026-11-07); auth, limits, CI and hardening notes updated for the 2026-11-16 security verification.

## Prerequisites

- [rustup](https://rustup.rs) — pins **Rust 1.99.1** in `rust-toolchain.toml` (edition 2024, `rust-version = "1.99"`).
- **Node 22.17+** (workspace `package.json` `engines`) for the dashboard.
- No Postgres. The server uses SQLite (`sqlite:monitor.db` by default, git-ignored).
- Optional: Playwright, for `scripts/e2e.sh`.

On macOS, put rustup first if the toolchain check fails:

```sh
export PATH="$(brew --prefix rustup)/bin:$PATH"
```

## Setup and run

```sh
git clone https://github.com/akira-shirase-oss/monitoring-service.git
cd monitoring-service

npm install                 # once
cargo build --locked        # Cargo.lock is already in the repo
scripts/dev.sh              # API on 127.0.0.1:58465 (SMON_DEV_PORT); serves the dashboard if built
```

In another shell:

```sh
monitor() { SMON_SERVER=http://127.0.0.1:58465 cargo run -q -p monitor-cli -- "$@"; }
monitor ping
# Loopback / private targets need the lab profile on the *server*, not the CLI.
# Start the server with: SMON_PROFILE=lab scripts/dev.sh
monitor monitors add example https://example.com/health
monitor monitors list
```

Dashboard options:

```sh
# hot reload; Vite on :58600 proxies /api, /livez, /readyz to the dev server on :58465
npm run dev --workspace @service-monitor/dashboard

# or build once and let the dev server serve it at 127.0.0.1:58465
npm run build --workspace @service-monitor/dashboard
```

Verify:

```sh
scripts/check.sh          # toolchain, fmt, clippy -D warnings, cargo test, cargo-deny (advisories, bans, licences, sources), architecture, svelte-check, dashboard build, npm audit
scripts/e2e.sh            # Playwright against the real server (separate; waits on a real 3-failure sequence)
scripts/backup-drill.sh   # backup → destroy → restore
```

Defaults (all overridable with `SMON_*` env vars; listed in the README):

| Variable | Default |
|---|---|
| `SMON_LISTEN` | `127.0.0.1:58400` |
| `SMON_DATABASE_URL` | `sqlite:monitor.db` |
| `SMON_DASHBOARD_DIR` | `apps/dashboard/build` |
| `SMON_PROFILE` | unset or `production` (SSRF guard on). `lab` permits local targets for fixtures. Any other value refuses to boot. |
| `SMON_ADMIN_TOKEN` | unset is allowed only on loopback; a tokenless public bind refuses to boot. When set: at least 32 bytes, no whitespace or `#`, or the boot refuses (`openssl rand -hex 32`) |
| `SMON_SESSION_TTL_S` / `SMON_SESSION_IDLE_S` | `43200` / `3600` — a dashboard session's lifetime, and how long it may sit unused |

A useful local lab:

```sh
node fixtures/fault-server.mjs          # 127.0.0.1:58700
# routes: /ok, /fail, /status/<n>, /slow?ms=<n>, /flaky?fail=3&pass=2, /hang,
#         POST /sink (records webhook body + headers), GET /sink/events
```

Then start the server with something like:

```sh
SMON_PROFILE=lab \
SMON_WEBHOOK_URL=http://127.0.0.1:58700/sink \
SMON_WEBHOOK_SECRET=test-secret \
scripts/dev.sh
```

and add monitors with short intervals, e.g. `monitor monitors add ok http://127.0.0.1:58700/ok --interval_s 5 --timeout_s 2`.

**What was verified on a Linux box (2026-11-10, America/Toronto):** `cargo test --workspace --locked` on Rust 1.99.1 passed in full (domain, store, API, probes, deliverer, email, schedule, snapshot, CLI exit codes). The dashboard `npm install` / Playwright suite was not run here.

## Happy-path checklist

- [ ] `monitor ping` sees `/livez` and `/readyz`: exit 0 when ready, 5 when degraded (`/readyz` 200 `"status":"degraded"` with `faults`), 1 when unavailable (`/readyz` 503).
- [ ] Add a monitor, `monitor monitors list` / `show <id>` shows it, and checks start landing (default interval is 60s unless you pass `--interval_s`).
- [ ] The dashboard loads (dev server on :58600, or the built app on :58465), lists the monitor, and the uptime strip leaves unobserved slots as gaps rather than filling them in green.
- [ ] Pause / unpause works; editing a field bumps the generation (`monitor monitors show` / the API).
- [ ] Optional: `scripts/check.sh` is green.

CLI surface (talks to the same HTTP API as the dashboard; `--server` / `SMON_SERVER` default `http://127.0.0.1:58400`, `--token` / `SMON_ADMIN_TOKEN` sent as `Authorization: Bearer`):

```
monitor ping
monitor monitors add|list|show|edit|pause|unpause|delete
monitor maintenance add|list|rm
```

`delete` only works on a monitor that has never recorded a check or incident — otherwise pause it. Exit codes: 0 success, 1 API error, 2 usage, 3 unreachable, 4 unauthorized.

## Try to break it

**Evidence stays honest**

- Point a monitor at the fault server's `/hang` or `/slow?ms=20000` with a 2s timeout. The outcome should be `timeout`, not a pass and not a generic transport error.
- `/fail` or `/status/500` should be `http_fail`, a refused port `transport`. Those classes should not collapse into each other.
- After a restart, previously committed checks, open incidents, and the outbox should still be there (`restart_rebuilds_streaks_and_open_incidents_from_the_store` in the store tests).
- After the server was down for longer than a monitor's interval (D-014), boot writes one `missed` row per monitor at the first slot that never ran: its status reads `unknown` until the next probe, and failures before and after the outage do not add up to an incident (`downtime_is_a_gap_failures_either_side_are_not_consecutive`).
- Unobserved slots in the dashboard should stay gaps, never interpolated green. A monitor with no committed evidence should read `unknown`, not "all down."

**Incidents and signed alerts**

- Use `/flaky?fail=3&pass=2` (or `--failures_to_open 3 --passes_to_recover 2`). Three consecutive failures should open an incident; two fresh passes should resolve it. Check times: `first_fail_at`, `last_fail_at`, `opened_at`, `resolved_at`.
- With `SMON_WEBHOOK_URL` / `SMON_WEBHOOK_SECRET` set, `GET http://127.0.0.1:58700/sink/events` should show deliveries with `x-smon-event` and `x-smon-signature`. The signature is **HMAC-SHA256 of the raw JSON body**, header form `sha256=<hex>`. A tampered body must not verify. Events should survive a server restart until delivered.
- A maintenance window (`monitor maintenance add <id> --from now --to <rfc3339>`) should suppress the page but still record checks.

**SSRF / policy**

- Without `SMON_PROFILE=lab`, adding (or editing a monitor to) `http://127.0.0.1:...` or another loopback, private or reserved literal — CGNAT `100.64.0.0/10` included — is rejected at registration: HTTP 422, code `refused_target`. Nothing is stored.
- A *name* that resolves to a local address is refused at probe time and recorded as `transport` + `rejected_local` — a failure that opens an incident and pages (D-013).
- Redirects *are* followed, up to 5 hops, and every hop is revalidated (`docs` and the probe tests). A redirect to a private *literal* stops there and the 3xx stands as `http_fail`; a redirect to a local *name* (`http://localhost/…`) is refused at the resolver and recorded as `transport` + `rejected_local`, which pages. A redirect loop should end as `http_fail`. (Note: `fixtures/README.md` still says a 301 should prove the probe does *not* follow redirects — that disagrees with the main README and the tests.)

**Auth and request limits**

- On loopback with no token, `/api` is open (dev mode). Set `SMON_ADMIN_TOKEN` and confirm a missing/wrong bearer is denied (CLI exit 4) — on every `/api` route and method, not just `GET /api/monitors` (`tests/auth_inventory.rs` walks them all).
- Binding a non-loopback address without `SMON_ADMIN_TOKEN` should refuse to boot and bind nothing.
- Many parallel wrong `POST /api/login` calls (or wrong `Authorization: Bearer` calls) from one address: five are evaluated, the rest get 429 with `Retry-After`, one more every 2 s. The owner's right token from another address is never refused. Behind a proxy set `SMON_TRUSTED_PROXY`, or every client shares the proxy's budget.
- Without `SMON_ADMIN_TOKEN` (dev mode) a non-loopback `Host` (a DNS-rebound page) gets 403, and a cross-site `Origin` gets 403.
- A request whose header never finishes, and an idle keep-alive, are closed after 10 s; a client that pipelines requests and never reads is closed after `SMON_WRITE_STALL_MS`; a connection is closed after 1000 requests or 5 minutes; past `SMON_MAX_CONNECTIONS` new connections wait (`perf/connections.mjs`).
- A store failure answers fixed text (`internal error`, `/readyz` → 503 `store unavailable`), never SQLite's message.
- A login body over 16 KiB gets 413 (even with the right token inside it); a request whose body never finishes gets 408 after 10 s.
- Sign in on the dashboard, then restart the server with a different `SMON_ADMIN_TOKEN`: the old session cookie must get 401. A session left unused for `SMON_SESSION_IDLE_S` ends too. The `sessions` table (and any backup) holds digests, never the cookie's value.
- `POST /api/backup` onto an existing file is 422 and leaves it alone; a new backup file is mode 0600.
- Responses carry `Content-Security-Policy`, `X-Content-Type-Options: nosniff`, `X-Frame-Options: DENY` and `Referrer-Policy: same-origin`; framing the dashboard should be refused by the browser.
- A webhook receiver that answers 307 to somewhere else: the alert is not re-sent there, and the outbox row stays pending.

## How to report a bug

Send Vinay (`akira-shirase-oss` / @404humandev) a note, or open a GitHub issue on **`monitoring-service`** (not `service-monitor`), including:

- Commit SHA (`git rev-parse HEAD`).
- OS, `rustc --version`, `node --version`.
- `SMON_*` you set (you can redact a real webhook secret; say whether one was set).
- The exact CLI/API calls, the monitor id, recent checks / incident fields, and whether the dashboard or the CLI disagreed.
- For alert bugs: the raw webhook body, `x-smon-event`, `x-smon-signature`, and how you verified (or failed to verify) the HMAC.
- For probe-class bugs: the target URL, expected outcome (`pass` / `http_fail` / `transport` / `timeout` / `missed`), and the stored row.

## Setup notes we hit

- Licensed MIT (`LICENSE`, 2026-11-16); the crates are not published to crates.io.
- CI: `.github/workflows/ci.yml` runs `scripts/check.sh` on ubuntu-latest for every push and pull request, plus a daily advisory sweep; `.github/dependabot.yml` covers cargo, npm and the action pins.
- Every drill and perf harness uses its own ports (never 58400): `scripts/dev.sh` 58465, `scripts/backup-drill.sh` 58460/58461, Playwright 58410/58710 (`SMON_E2E_SERVER_PORT`/`SMON_E2E_FAULT_PORT`), `perf/*.mjs` per `perf/README.md`.
- `Cargo.toml` `repository` points at `https://github.com/akira-shirase-oss/service-monitor`, which is not this repo.
- `fixtures/README.md` is stale on redirects (see above).
