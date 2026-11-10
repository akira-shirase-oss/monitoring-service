# Test report: Service Monitor (`monitoring-service` / service-monitor)

**Commit tested:** `fb3e966` (guide written against this HEAD)
**Date:** 2026-11-10 · **Machine:** Apple M2 Pro, 12 cores, 32 GB, macOS (Darwin 27.0.0)
**Toolchain:** Rust 1.99.1 via rustup proxies (pin enforced by `check-toolchain.sh`)
**Guide:** `test/monitoring-service-TESTING.md`
**Verdict: PASS — no new bugs.** The lab run verified every contract end-to-end; the
24h soak is **in progress** (see §8).

---

## 1. Build and gates

| Check | Result |
|---|---|
| `scripts/check.sh` (toolchain, fmt, clippy `-D warnings`, `cargo test --workspace --locked`, architecture check, svelte-check, dashboard build) | **green** |
| `cargo test --workspace --locked` | **137 tests listed, all pass** — incl. `three_consecutive_failures_open_an_incident`, `open_incident_survives_a_restart`, `maintenance_windows_create_list_and_cancel`, `every_incident_word_is_in_the_guide` |
| Dashboard | `vite build` clean; served by monitor-server on same origin |

## 2. Lab environment

`SMON_PROFILE=lab` monitor-server on `127.0.0.1:58400` (SQLite at `/tmp/smon-test.db`)
+ fault fixture server on `127.0.0.1:58700` (`/ok`, `/fail`, `/hang`,
`/slow?ms=20000`, `/flaky?fail=3&pass=2`, `/sink` webhook recorder). Five monitors
added at 15 s intervals — the lab profile is what permits loopback targets.

## 3. Probe outcomes — every classification verified live

| Fixture | Expected | Observed |
|---|---|---|
| `/ok` | pass | `pass (HTTP 200)`, ~1 ms |
| `/fail` | http_fail | consecutive `http_fail` checks → incident opened at 3rd failure |
| `/hang` | timeout (~2 s deadline) | `timeout`, incident opened — not a pass, not a generic transport error |
| `/slow?ms=20000` | timeout | `timeout` at the 2 s deadline, incident opened |
| `/flaky?fail=3&pass=2` | incident opens *and* recovers per thresholds | opened after 3 fails; **recovered** after 2 fresh passes; opened again on next fail cycle — incident machine cycles honestly |

Missed slots display `unknown`, never counted as passes; open incidents persisted
until two fresh passes closed them.

## 4. Signed webhooks — HMAC verified, tamper rejected

- `/sink/events` recorded **6 deliveries** (`incident_opened` ×5, `incident_recovered` ×1)
  with `x-smon-event` + `x-smon-signature: sha256=<hex>` headers.
- **6/6 signatures verify** as `HMAC-SHA256(raw body, 'test-secret')`.
- **Tampered body vs original signature → verifies: False.**
- Events survive server restart until delivered (outbox is SQLite-authoritative).

## 5. SSRF / egress policy

Second server instance **without** `SMON_PROFILE=lab`:

| Monitor | Result |
|---|---|
| `http://127.0.0.1:58700/ok` (reachable loopback) | **down** — guard refuses the probe despite reachability |
| `http://192.168.1.1/` (private) | **down** |
| `https://example.com/` (public) | **up** — real egress works |

The guard holds at probe time: loopback/private targets can be *created* but can
never be successfully probed outside lab profile.

## 6. Management auth and bind policy

| Check | Observed |
|---|---|
| `GET /api/monitors` no credentials | **401** |
| wrong bearer token | **401** |
| right `SMON_ADMIN_TOKEN` | **200** |
| CLI call without token | `unauthenticated: a valid Bearer token or session cookie is required`, **exit 4** |
| CLI call, wrong token | same, exit 4 |
| bind `0.0.0.0` **without** token | **refuses to boot**: `Error: SMON_ADMIN_TOKEN is required when listening on 0.0.0.0:58403` |
| `/api/login` + `/api/logout` | session-cookie path present for the dashboard |

## 7. Monitor lifecycle, maintenance, restart, backup

- **Pause** → generation bumps to 2, checks stop, history retained; **edit** interval
  15 s→30 s → generation 3; **unpause** → generation 4 (generation invalidation live).
- **Delete a monitor with recorded history** → refused:
  `conflict: monitor has recorded history; pause it instead`.
- **Maintenance window** on the flaky monitor → `suppressing`; the incident still
  opened in evidence but **sink deliveries stayed at 7** — the page was suppressed,
  the evidence was not.
- **Kill -9 + restart**: all 5 monitors and 5 incidents (open + resolved states)
  survived; statuses rebuilt from committed evidence.
- **Backup-drill** (`scripts/backup-drill.sh`, the M4.3 rehearsal): incident driven
  open → `POST /api/backup` (VACUUM INTO, 86,016 B) → kill server → restore →
  `status=down open_incidents=1 pending_outbox=1` → **`DRILL PASSED`**.

## 8. Load evidence

- **Committed prior runs**: 24h soak 2026-11-06 (salvaged: 576k checks, 0 missed,
  13.2k alerts), incident-storm (40/40 incidents, probes unstarved), session-churn
  (94k login/read/logout cycles in 90 s, sessions bounded), real-app drill 3.84M
  requests — all under `perf/results/`.
- **Known prior failure, already fixed**: that soak exposed a summarizer
  `maxBuffer` crash (`fb3e966`) — the fix is in the tested commit.
- **In progress**: a fresh 24h `lab-profile` soak (100 monitors @15 s, mixed
  ok/flaky/slow/dead/hung fixtures) launched 2026-11-10 ~10:15, due ~2026-11-11
  ~10:15. Result JSON lands in `perf/results/`; gates to be verified on completion.
  This report will be updated with its outcome.

## 9. Failures found

**None new.** Every guide contract held on first execution. The only failure in this
project's recent history — the soak summarizer `maxBuffer` crash — was found by the
Nov 6 soak, fixed in `fb3e966` (the commit under test), and is exactly why the
current re-run exists.

## 10. Verdict

Service Monitor passes the external testing guide at `fb3e966`: 137 tests + full
`check.sh` gate green; lab probes classify every outcome honestly; incidents open
and recover per thresholds; webhooks are genuinely HMAC-signed and tamper-evident;
SSRF, auth, bind policy, maintenance suppression, restart recovery and backup
restore all behave as specified. The 24h soak — the last owed evidence — is running
now; this report stands at PASS for everything testable today.
