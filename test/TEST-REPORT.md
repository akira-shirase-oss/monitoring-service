# Test report: Service Monitor (`monitoring-service` / service-monitor)

**Commit tested:** `fb3e966` (guide written against this HEAD)
**Date:** 2026-11-10 · **Machine:** Apple M2 Pro, 12 cores, 32 GB, macOS (Darwin 27.0.0)
**Toolchain:** Rust 1.99.1 via rustup proxies (pin enforced by `check-toolchain.sh`)
**Guide:** `test/monitoring-service-TESTING.md`
**Verdict: PASS — no new bugs.** The lab run verified every contract end-to-end;
24h soak evidence exists (salvaged run, all gates true) and the clean re-run was
descoped at 78 min with partial evidence committed (see §8).

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
- **Descoped re-run (2026-11-10)**: a fresh 24h `lab-profile` soak (100 monitors
  @15 s, mixed ok/flaky/slow/dead/hung fixtures) launched ~10:15 was cut at
  **78 min** on the maintainer's call — a full second 24h run was judged not
  worth the machine time now that a complete 24h run exists (salvaged above).
  The summarizer only writes its JSON at the end of the full duration, so the
  partial run's metrics were reconstructed from its preserved DB:
  `perf/results/soak-24h-2026-11-10T15-15-42.585Z-84a6811-descope.json` —
  31,300 checks, **0 missed slots**, honest outcome mix (24,305 pass /
  5,430 http_fail / 1,565 timeout by fixture design), 374 incidents opened /
  357 resolved / 17 open at kill, commit-lag p50 2.87 s / p95 4.92 s,
  outbox 728 delivered + 3 pending-at-kill (0 attempts — enqueued in the final
  transaction, would drain on restart), 7.4 MB storage. All computable gates
  pass; `outbox_drained` reads false only because the run was killed mid-flight.
  RSS series and sink-hit count were in process memory and are lost (same
  limitation as the salvaged run).

## 9. Failures found

**None new.** Every guide contract held on first execution. The only failure in this
project's recent history — the soak summarizer `maxBuffer` crash — was found by the
Nov 6 soak, fixed in `fb3e966` (the commit under test), and motivated the clean
re-run that was later descoped at 78 min (§8).

## 10. Verdict

Service Monitor passes the external testing guide at `fb3e966`: 137 tests + full
`check.sh` gate green; lab probes classify every outcome honestly; incidents open
and recover per thresholds; webhooks are genuinely HMAC-signed and tamper-evident;
SSRF, auth, bind policy, maintenance suppression, restart recovery and backup
restore all behave as specified. 24h soak evidence exists (the salvaged Nov 6–8
run, all gates true); the clean re-run was descoped at 78 min on the maintainer's
call and its partial evidence is committed. This report stands at PASS.
