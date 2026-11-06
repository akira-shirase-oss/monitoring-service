# Roadmap

Milestones in dependency order. Each step states the user outcome, the invariant, the failure
case it must survive, and the acceptance test — one step per branch, merged when its evidence
exists. Modules referenced live in `docs/modules/`.

## M0 — Scaffold (done, commit ab00fbf)

| Step | Outcome | Evidence |
|---|---|---|
| 0.1 workspace + toolchain + gates | reproducible repo | `scripts/check.sh` |
| 0.2 domain + incident state machine | contract as pure code | 8 unit tests |
| 0.3 store + API + CLI + dashboard shell | end-to-end management path | 3 API tests, live run |

## M1 — Check and remember *(done)*

One typed check on a bounded schedule, durable history, restart-safe.

| Step | Outcome | Invariant | Failure case | Acceptance test |
|---|---|---|---|---|
| 1.1 probe engine (module 03) | a probe returns a classified `CheckOutcome` | deadline and body bound are hard; target policy enforced before connecting | hung target → `timeout`, not a stalled worker | 7 fixture tests in `tests/probe.rs` |
| 1.2 scheduler + coordinator | slots fire per interval, results collected | one attempt per slot, no overlap, no catch-up; generation drop | saturated workers → `missed`, never a queue storm | 3 end-to-end tests in `tests/schedule.rs` |
| 1.3 db-writer task (module 02) | observations land transactionally | check + incident transition + outbox = one tx | writer dies → pending results dropped, coverage gap visible | tx + drop tests in `tests/store.rs` |
| 1.4 restart recovery | open incidents and streaks rebuild from the store | incident persists until two *fresh* passes | kill mid-incident → still open, streaks rebuilt | `Health::rebuild` unit tests + live restart |
| 1.5 status surfacing | API + dashboard show real status | displayed status only from committed evidence | unwritten result never shows | statuses map written post-commit only |

**M1 exit — verified live:** registered a monitor against a fixture; observed `up` after the
first probe; killed the target → three `transport` checks → incident `open` +
`incident_opened` outbox in one commit; restarted the server mid-incident → still `down`;
target back → two fresh passes → `resolved` + `incident_recovered`. First useful release.

## M2 — Notify *(done)*

| Step | Outcome | Invariant | Failure case | Acceptance test |
|---|---|---|---|---|
| 2.1 outbox + webhook deliverer (module 04) | one logical event delivered at-least-once | stable `event_id`; finite retries + jitter | receiver hangs → delivery pool isolated, probes continue | 6 integration tests in `tests/deliver.rs` (hang, retry, exhaustion, restart, signature) |
| 2.2 edit/pause/delete monitors | management writes bump generation | stale results invalidated | pause mid-probe → late result dropped | `a_stale_observation_after_edit_is_dropped` + patch/delete edge tests in `tests/api.rs` |
| 2.3 incidents API + dashboard detail | incident timeline + recent checks visible | success vs coverage kept separate | reordered views show evidence times | `checks_and_incidents_endpoints_answer`; detail page live |
| 2.4 fault fixtures v1 (module 09) | Node fault server: healthy/slow/fail/hang | deterministic scenarios | n/a | `fixtures/fault-server.mjs` + `/sink` receiver |

**M2 exit — verified live:** `/flaky?fail=3&pass=2` monitor cycled incidents open/resolved;
`incident_opened` and `incident_recovered` both delivered to `/sink` with `x-smon-event` +
verified `sha256=` signature, first-attempt `delivered_at`. CLI `edit`/`pause`/`unpause`
bumped generation 2→3→4; stale-`generation` PATCH → `conflict`; delete-with-history →
`conflict`. Webhook pointed at `/sink/hang`: API stayed responsive, probes kept landing,
outbox attempts grew bounded (retries scheduled, no deliveries) — a hung receiver occupies a
delivery slot, never a probe slot.

*Noted during the run:* HTTP clients may transparently retry idempotent requests on stale
keep-alive connections — the fixture consumes a phase per *request on the wire*, which need
not equal checks-per-slot. Harmless here (GET-only, observation = last response) but worth
remembering if a fixture ever counts hits.

## M3 — Display and public status

| Step | Outcome | Invariant | Failure case | Acceptance test |
|---|---|---|---|---|
| 3.1 coverage vs success (module 06) **(done)** | `GET …/stats` returns expected/observed/missed/passed + basis-point ratios; detail page shows coverage and success separately (`null` → "no data") | missing slots never count as passes; undefined denominators stay `null` | 24 h of misses shows 0% coverage, not "down by silence" | `CoverageStats` domain tests, `check_stats` store test, `stats_separates_coverage_from_success` API test; verified live (flaky → 0% success, keeper → 100%) |
| 3.2 public status snapshot + renderer (module 07) **(done)** | `status.json` snapshot rewritten every beat (tmp+rename); `apps/status/index.html` static renderer | allowlist fields only (sha256 alias, status, incident time); TTL declared in-file | stale snapshot says so | allowlist/TTL unit tests + writer-beats-file test |
| 3.3 independent heartbeat **(done)** | `fixtures/status-check.mjs` decides from `expires_at`; page shows "monitoring unavailable" on expiry | "monitoring unavailable" ≠ "all down" | monitor host stops → heartbeat expires | verified live: kill → `unavailable` (exit 1), restart → `alive` |
| 3.4 Playwright suite **(done)** | `tests/dashboard.spec.ts` + `scripts/e2e.sh`: API-abort banner, badge on committed evidence, real incident on detail page, keyboard nav | unreachable server state renders | server down mid-session (route abort) | 4 specs, verified run |

## M4 — Operate

| Step | Outcome | Invariant | Failure case | Acceptance test |
|---|---|---|---|---|
| 4.1 retention: 7-day raw + aggregates **(done)** | `check_stats_daily` (migration 0002); `writer.retain` rolls expired rows into per-day outcome counts and deletes them in one transaction; task ticks hourly (`SMON_RETENTION_S`, `SMON_RETENTION_INTERVAL_S`) | live data never expires; `check_stats` unions raw + aggregate so coverage stays honest | expiry never touches open incidents (raw rows spared while `state='open'`) or the outbox | `retention_aggregates_then_deletes_and_spares_open_incidents`; live: 3 expired → aggregate, stats union shows 6 observed |
| 4.2 disk guardrails + log rotation **(done)** | `SMON_MAX_DB_BYTES` (default 2 GiB) → `PRAGMA max_page_count` on connect; `BoundedLog` rotating `SMON_LOG_FILE` at `SMON_LOG_MAX_BYTES` (2 generations) | pressure → fail-safe, not corruption: writes past the ceiling fail cleanly | full disk → commits fail → writer's consecutive-failure counter → `/readyz` 503 `unavailable`; the counter resets only on a landed write | `writes_past_the_page_ceiling_fail_and_flag_unhealthy`, `a_landed_write_clears_the_failure_count`, `readyz_reports_unavailable_when_writes_fail`, `logfile` rotation tests; live: capped DB → 503, log rotated to `.1` |
| 4.3 backup + restore drill **(done)** | `Db::backup` (`VACUUM INTO`, WAL-safe online snapshot) + `POST /api/backup {path}` + `scripts/backup-drill.sh` | restored open incidents stay open; pending outbox stays pending | restore mid-incident → `down` + incident + outbox intact | `backup_restore_preserves_open_incidents_and_outbox`, `backup_writes_a_snapshot_to_an_absolute_path`; live drill PASSED |
| 4.4 deploy off target host **(done)** | `scripts/deploy.sh` → `releases/<rel>/` + `current` symlink, `data/` outside releases; `deploy/` systemd unit + launchd plist + `RUNBOOK.md` | separate probe host, status host (status dir syncs to a static host; `data/` never under a release) | upgrade mid-incident → boot rebuild resumes `down`; rollback = repoint + restart, schema too-new fails the migration check honestly | live drill: v1 → incident open → v2 → still down → rollback to v1 → still down |

## M5 — Ship

| Step | Outcome | Invariant | Failure case | Acceptance test |
|---|---|---|---|---|
| 5.1 management auth **(done)** | `SMON_ADMIN_TOKEN` → `Bearer` required on every `/api/**` (sha256-digest compare); `/livez` `/readyz` statics stay open; dashboard TokenGate + CLI `--token` | unauthenticated = denied — once enabled, no bypass | credential failure → 401; tokenless non-loopback bind refuses to boot | `auth_denies_without_a_valid_bearer_token` (5 deny shapes + accept + open health); live: 401/401/200, CLI denied/allowed, boot refused |
| 5.2 SSRF complete **(done)** | `PolicyResolver` validates every DNS answer inside reqwest's resolver (dialer only sees validated addrs — no TOCTOU gap); redirects followed ≤5 hops, each revalidated; DNS refusal classifies `rejected_local` | a single local address in an answer fails the whole resolution | public name → private IP refused; refused/looping redirect leaves the 3xx as `http_fail` | unit: `is_local` full-range matrix incl. v4-mapped v6, `validate_addrs` mixed answer, real `localhost` refusal; live: `localhost` target → `transport`/`rejected_local`, redirect chain → `pass`, loop → `http_fail/302` |
| 5.3 load runs **(done)** | `perf/lab-profile.mjs` baseline: 100 monitors @ 15 s phase-locked (built-in 6.7× burst/wave), 10 fixture origins, 10 min — 4000/4000 slots, 0 missed, commit-lag p95 4.9 s, 107/107 alerts first-attempt, RSS flat | bounded pools hold under a 22.5% error rate; misses counted, never hidden | wave drain is the whole lag story; per-origin bound correctly throttles a single-origin run to a trickle (first run proved it) | `perf/results/2026-11-06T01-18-54.904Z-33656f5.json`; 24 h soak not yet run — documented, not claimed |
| 5.4 release **(done)** | installable artifact + docs; fresh-prefix install drill passed (migrations → livez/readyz/`/version` → dashboard → auth) | honest claims only | n/a | e2e gate fixed (config-less playwright run); 4/4 specs + install drill |

**Ship gate:** another developer installs it, gets a useful alert, and distinguishes a target
outage from missing monitoring data.

## M6 — Parity + UI (gap log: `docs/GAPS.md`)

| Step | Outcome | Invariant | Failure case | Acceptance test |
|---|---|---|---|---|
| 6.1 TLS classification **(done)** | `transport` detail: `cert_expired` / `cert_not_yet_valid` / `cert_name_mismatch` / `cert_untrusted` / `tls`; `ClientConfig` on `webpki-roots` + `ProbePolicy.extra_roots` (D-010) | a cert failure never reads as `connect`; committed detail is a fixed string, never raw error text | platform verifier flattens cert errors (macOS Security.framework) → bypassed by deterministic trust | 5 rcgen/tokio-rustls fixture tests; live: `expired.badssl.com` → `cert_expired`, `wrong.host.badssl.com` → `cert_name_mismatch` |
| 6.2 per-monitor thresholds | `fail_threshold` / `recover_threshold` on the monitor, validated, honored by the incident machine | optional; defaults keep 3/2 | invalid values rejected at create/edit | threshold matrix tests + live flappy monitor |
| 6.3 incident four times | `first_fail_at` / `last_fail_at` / `opened_at` / `resolved_at` tracked separately | start-of-failure ≠ time-of-notice | backfilled rows stay honest (NULL ≠ fabricated) | migration + incident test asserts all four |
| 6.4 dashboard mutations | add/edit/pause/delete in the UI behind the token | UI never bypasses the API contract | 401 → token gate, not silent failure | Playwright mutation spec |
| 6.5 charts | uptime timeline/sparkline from real check rows | chart renders `unknown`/no-data honestly | sparse data shows gaps, not interpolated green | component test + spec |
| 6.6 auto-refresh | poll list/detail without losing honest states | refresh never flashes a stale green | API down mid-poll → unreachable state | spec with route abort |
| 6.7 public status view | render sanitized `status.json` inside the dashboard | public viewers never touch management APIs | expired heartbeat → "monitoring unavailable" | spec + drift check vs schema |
| 6.8 guide page | plain-English status/word reference | drift-checked against real enums | a new outcome not in the guide fails the check | drift test |
| 6.9 CLI exit codes | contract audit + pinned tests | scripts can trust `$?` | every failure mode maps to a documented code | exit-code matrix tests |
| 6.10 mutation testing | cargo-mutants on the domain crate | tests must kill mutants, not just pass | survivors triaged: fix or justify | mutants run logged |
| 6.11 maintenance windows | schedule-scoped suppression: checks record, incidents don't open during a window | suppression is explicit, time-bounded, never silent | window expiry resumes normal alerting | window matrix tests + live drill |
