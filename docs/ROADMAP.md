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
| 6.2 per-monitor thresholds **(done)** | `failures_to_open` / `passes_to_recover` (`1..=10`, default 3/2) on create+patch+CLI; migration 0003; the writer reads them from the same `monitor_gate` row as the generation — an edit applies atomically | the machine never runs on stale config; defaults keep 3/2 | out-of-range → `invalid_request`; flag still wins over replay disagreement | domain matrix (1/5/edge/validation), store: strict opens on 1st failure + edited recover honored, API create/patch/defaults; live: fail=1 monitor → open after 1 check, default still waiting |
| 6.3 incident four times **(done)** | `first_fail_at` / `last_fail_at` / `opened_at` / `resolved_at` tracked separately; migration 0004 | start-of-failure ≠ time-of-notice: `first_fail_at` is derived inside the commit transaction from committed check rows (oldest consecutive failure since the last pass/missed), so it stays correct across restarts; `last_fail_at` re-stamps on every failing check while open | backfilled/pre-M6.3 rows keep NULL — never fabricated history | store test: streak slots 1–5 → first=slot1, opened=slot3, last=slot5, resolved=slot7; API `IncidentView` + webhook `IncidentRef` carry all four; live: refused-port monitor → first_fail 30 s before opened |
| 6.4 dashboard mutations **(done)** | add via shared `MonitorForm`; detail pause/resume, edit (interval/timeout/thresholds + `generation` guard), delete behind two-click confirm | UI never bypasses the API contract — every write is the same envelope | 409s surface verbatim (stale-generation, delete-with-history "pause it instead"); 401 → token hint | Playwright: UI create → pause (API-verified) → edit (gen 1→2→3 per write) → delete; history-refusal message; 401 spec |
| 6.5 charts **(done)** | `CheckStrip` uptime strip on the detail page — one cell per committed check (status tokens) + dashed gap cells | chart renders `unknown`/no-data honestly: `buildStripCells` inserts gap cells where slot-grid distance shows unobserved time | pause mid-run → the window is truly empty → `data-outcome="missing"` cells, never interpolated green | Playwright: pass/http_fail cells on up/down monitors; pause-gap spec (pause ~2 slots → resume → gap cell rendered) |
| 6.6 auto-refresh **(done)** | `invalidateAll()` on the 15 s tick on both pages; hidden tabs skip | refresh never flashes stale green — SvelteKit holds last data until reload resolves, atomic swap | API down mid-poll → "cannot see" via the same load path | Playwright: new monitor appears on the tick without reload; route-abort mid-poll → unreachable banner (both ~15.6 s, one tick) |
| 6.7 public status view **(done)** | `GET /status.json` serves the same file unmodified (outside `/api`, no token); `/status` route renders it with app components | public viewers never touch management APIs — the artifact IS the contract | expired heartbeat → "monitoring unavailable" + stale rows; missing → same verdict, no rows | Playwright: live snapshot spec + stale-doctor spec + 404 spec; schema pinned by `allowlist` unit test |
| 6.8 guide page **(done)** | `/guide` renders one glossary (`lib/vocabulary.ts`) — statuses/outcomes/details/incidents + concepts | guide words ⊆ real wire vocabulary AND real enums ⊆ guide (both directions checked) | a new outcome/status/detail/incident word not in the guide fails `guide_drift.rs`; a stale guide word fails too | 5 Rust drift tests (through the store's own `encode_slot`) + Playwright section/word spec |
| 6.9 CLI exit codes **(done)** | audited + pinned: 0 ok · 1 API/app error · 2 usage · 3 unreachable · 4 unauthorized | scripts can trust `$?` — "server down" is distinguishable from "server refused" | every failure mode maps to a documented code; stderr for errors, stdout for results | `tests/exit_codes.rs` spawns the real binary against a std stub HTTP server: 7 cases |
| 6.10 mutation testing **(done)** | cargo-mutants on the domain crate: 48 mutants → 42 caught, 6 unviable, **0 missed** | tests must kill mutants, not just pass | first run missed 5 boundary mutants — three tests added (interval==MIN accepted, timeout==interval accepted, rebuild_tail sizing) killed all | `perf/results/*-mutants-domain.json` |
| 6.11 maintenance windows **(done)** | schedule-scoped suppression: `maintenance_windows` read inside the observation gate; crossing inside a window → `IncidentSuppressed` (check commits, no incident/outbox) | suppression is explicit, time-bounded, never silent — the window row is the explanation | streak keeps counting: first post-expiry failure opens immediately; recovery during a window still closes | domain matrix (suppressed/pass-resets/missed-resets/recover-during-window) + store test + API CRUD test; live drill: 4 checks suppressed → `rm` → incident opens next slot with `first_fail_at` inside the window |

## M7 — Deferred items, closed

| Step | Outcome | Invariant | Failure case | Acceptance test |
|---|---|---|---|---|
| 7.1 email alerts **(done)** | `notify_outbox.channel` (migration 0006): one incident transition enqueues one row per configured channel — webhook always, email when `SMON_SMTP_{DSN,FROM,TO}` are all set; `Sink::Email` (lettre/rustls+tokio) delivers through the same bounded pool, deadline, deterministic retry and writer bookkeeping | each channel owns its delivery; a dead relay can never claim or stall another channel's rows; credentials env-only, never logged | partial SMTP config → boot error, not a half-live channel; relay refusal → retry under the same event_id; subject CR/LF stripped (header injection never reaches the wire); `Message-ID` threads retries | store: one row per channel, `pending_outbox` filters per channel; integration: real SMTP stub on loopback — delivered+marked, 550→retry→delivered (attempts=2), webhook row untouched; live drill: real server → stub relay captures wire, email row `delivered_at` set |
| 7.2 dashboard login UX **(done)** | `POST /api/login` exchanges the admin token for a `sessions` row (migration 0007) + HttpOnly `SameSite=Strict` cookie (`SMON_SESSION_TTL_S`, default 12 h); `POST /api/logout` kills the row + expires the cookie; bearer unchanged for CLI; `TokenGate`→`LoginGate` + nav Sign out | store-side `expires_at` is the authority — a cookie can't extend its own life; session-store failure → 503, never a false 401 | wrong token → ~300 ms delay + 401; tokenless dev mode → 404 `auth_disabled`; expired/forged cookie → 401; cookie-authed mutation without/with wrong Origin → 403; mismatched Origin rejected for any credential | store: create/valid/expire/sweep; api: 7 session tests (cookie flags, Origin matrix, logout, ttl=0 expiry); Playwright: sign-in spec + all 16 specs now run the real auth path; live drill: 401→204+cookie→GET 200→POST 403/403/201→logout→401, 0 session rows |
| 7.3 real-app load drill | rising-load harness against a real Node app (not fixture endpoints); committed result | lab numbers stay lab numbers — the drill documents workload, error rate, lag, misses, alert correctness, memory/DB growth, limits | harness failures are evidence, not rerun-until-green | `perf/results/` artifact + module-09 note |
