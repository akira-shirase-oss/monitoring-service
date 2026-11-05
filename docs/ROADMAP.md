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
| 3.2 public status snapshot + renderer (module 07) | read-only public page, sanitized + expiring | no internals; snapshot has TTL | stale snapshot says so | snapshot content test |
| 3.3 independent heartbeat | external checker notices monitor death | "monitoring unavailable" ≠ "all down" | monitor host stops → heartbeat expires | simulated heartbeat expiry |
| 3.4 Playwright suite | dashboard flows verified in a browser | unreachable server state renders | server down mid-session | playwright specs |

## M4 — Operate

| Step | Outcome | Invariant | Failure case | Acceptance test |
|---|---|---|---|---|
| 4.1 retention: 7-day raw + aggregates | bounded history | live data never expires | expiry never touches open incidents/open outbox | seeded-retention test |
| 4.2 disk guardrails + log rotation | ~2 GiB ceiling, bounded logs | pressure → fail-safe, not corruption | full disk → writes fail, status shows unavailable | disk-pressure fixture |
| 4.3 backup + restore drill | documented restore that works | restored open incidents stay open | restore mid-incident | restore drill script + test |
| 4.4 deploy off target host | monitor runs independently | separate probe host, status host | upgrade/rollback | deploy runbook exercised |

## M5 — Ship

| Step | Outcome | Invariant | Failure case | Acceptance test |
|---|---|---|---|---|
| 5.1 management auth | management routes require it | unauthenticated = denied | credential failure → locked out, never open | auth tests |
| 5.2 SSRF complete (module 03) | redirect revalidation, DNS-level private-IP checks, lab profile | localhost only in lab profile | public name → private IP refused | SSRF fixture suite |
| 5.3 load runs (module 09) | measured lab profile | thresholds frozen from baseline | p95 dispatch lag < 5 s target, 24 h soak | k6 results committed |
| 5.4 release | installable artifact + docs | honest claims only | n/a | fresh-machine install by another dev |

**Ship gate:** another developer installs it, gets a useful alert, and distinguishes a target
outage from missing monitoring data.
