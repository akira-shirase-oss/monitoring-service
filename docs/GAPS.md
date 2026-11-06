# Gap log — rebuild vs the retired monitor

Every point the old Service Monitor covered, checked against the rebuild on
2026-11-06. Each gap gets a decision — **build** (M6), **defer** (documented, not
claimed), or **rejected** (out of scope, with the reason). Nothing here is silently
dropped.

Source: the retired build log (deployed at 404human.dev/service-monitor/ until the
rebuild log ships), mined before its removal from `minimal-portfolio`.

## Covered — rebuild is at parity or stronger

| Old point | Rebuild |
|---|---|
| SSRF disguises (0.3) | Stronger — DNS answers validated inside reqwest's resolver, v4-mapped v6 caught, `rejected_local` evidence |
| Failing fixture server (0.4) | fault-server: ok/fail/slow/hang/status/flaky/redirect/sink |
| One check, one try (rule) | One attempt per slot; `missed` counted; reqwest's pooled-connection retry noted in ROADMAP |
| Three-strike incidents (1.2) | 3-fail open, 2-fresh-pass recover |
| Dashboard admits unknown/STALE (1.3) | unreachable/auth/missing states + self-expiring status.json heartbeat |
| Fair queue (1.4) | 20 global / 2 per origin, `try_acquire` → missed; proven in the load baseline |
| Persistence + restart rebuild (2.1, 2.5) | Boot rebuild from committed evidence; upgrade/rollback drill keeps `down` |
| Retention (2.6) | Stronger — daily aggregates keep coverage true; open incidents keep raw rows |
| Disk limits + backups (2.7) | `SMON_MAX_DB_BYTES` → real 503; `VACUUM INTO` + restore drill |
| No green without proof | `unknown` first-class; coverage ≠ success everywhere |
| Signed webhooks + retries (old M3) | Done in rebuild M2 |

## Build — the M6 parity slice

| Gap | Old evidence | M6 step | Decision rationale |
|---|---|---|---|
| ~~TLS/certificate classification~~ | 2.8 | **M6.1 done** | `cert_expired` / `cert_not_yet_valid` / `cert_name_mismatch` / `cert_untrusted` / `tls` committed via deterministic webpki-roots trust (D-010) + `extra_roots`; rcgen fixtures + badssl live drill |
| ~~Per-monitor alarm sensitivity~~ | 2.3 | **M6.2 done** | `failures_to_open`/`passes_to_recover` per monitor (`1..=10`, default 3/2), read from the committed gate row so edits apply atomically |
| Incident four times kept apart | 2.4: opened / first-fail / last-fail / resolved | M6.3 | Cheap columns, real forensic value — when did it *start* failing vs when did we *notice* |
| Dashboard mutations | old M4 "adding/editing from the dashboard" | M6.4 | The token gate exists; the API contract exists; the UI can hold write authority behind the same credential now |
| Charts / uptime timeline | (dashboard richer than rows) | M6.5 | The check rows already carry everything a sparkline needs |
| Auto-refresh | — | M6.6 | A monitor page you must reload is weaker than its data |
| Public status view in-app | old M5 "status page somewhere else" | M6.7 | Render the *sanitized* status.json inside the dashboard — public viewers still never reach management APIs |
| Guide page | 2.2: plain-English status/word reference, drift-checked | M6.8 | Cheap; the honesty vocabulary needs explaining |
| CLI exit codes | 0.6 | M6.9 | Contract audit + pinned tests |
| Mutation testing | 0.7 | M6.10 | 88 hand-written tests have never been attacked; cargo-mutants on the domain |
| Maintenance windows | old M4 planned | M6.11 | Real feature: suppression schedule so planned work doesn't page; scoped as "no new incidents during window, checks still record" |

## Defer — documented, not claimed

| Point | Why deferred |
|---|---|
| Email alerts (old M3.4) | Webhook is the v1 channel; an SMTP adapter is a separate milestone with its own auth surface |
| Dashboard management login UX (old M4 "logging in") | Token gate exists; a session/login system is out of scope |
| 24-hour soak | Owed evidence, not a feature — `perf/lab-profile.mjs --duration-s 86400` |
| Real-app load watch (old 1.1) | The lab-profile harness covers bounded evidence; a real-Node-app rising-load run is a separate drill |

## Rejected — not rebuilding

| Point | Reason |
|---|---|
| No-JS dashboard (old 1.3) | The rebuild's dashboard is a client-side app by design; the *public* page (`apps/status`) stays JS-free-ish static HTML — the honesty guarantee lives there |
| Keeping deleted monitors (old 2.3) | Rebuild refuses delete-with-evidence instead — history is preserved *by not deleting*, which is the same guarantee |
