# Service Monitor load matrix — stability and scale evidence

Goal: committed evidence the monitor tells the truth under load — probes stay bounded, misses
are counted, incidents and alerts stay honest, the API and dashboard reads hold — per the
guide and the workspace ≥1M standard (units: probe checks / API requests where applicable).

Honesty: numbers are development observations on this machine, not production-capacity claims;
saturation shape and failure behavior are the evidence. `SMON_PROFILE=lab` permits the loopback
fixtures; never on a public bind.

| Module | Scenario | What it proves | Evidence status |
|---|---|---|---|
| Probe engine | `perf/lab-profile.mjs` | 100 monitors @ 15s phase-locked, 10 fixture origins — bounded pools, counted misses, commit lag | committed (4000 checks, 0 missed, p95 lag 4.9s); 2026-11-17 ceiling probe (200 `/ok` monitors, release, `304edd5`): 800 pass + 800 missed — see below; **≥1M checks not run**; **post-D-018 ceiling re-run 2026-11-19, release, `078cfed`: 200 `/ok` monitors, 1,574 committed checks, 0 missed, lag p95 2.51 s** — D-018's phase spread holds |
| End-to-end truth | `perf/real-app.mjs` + `real-app-load.mjs` | real Node app under rising load with `__ctl` degradation — monitor's readings vs app's own ground truth | committed (3.84M requests, 0 missed, incident times honest, both alert channels) |
| Failure/incident storm | `perf/storm.mjs` | N targets fail together → N incidents + outbox rows in bounded transactions; delivery pool never starves probes | **post-D-018 re-run 2026-11-19, debug, `078cfed`** (see below): 40 dead → 40 incidents, survivor kept checking (20), 4,582 API reads 0 failures — PASS |
| API read surface | `perf/api-load.mjs` (k6 `perf/k6/api-reads.js`) | `/api/**` reads at sustained rate while probes run — token auth, RLS-free SQLite reads | **committed 2026-11-17, release, `304edd5`: 1,000,000/1,000,000 answered 200 (gate passed), 1,647 rps @32 VUs, p50 15.7 / p95 30.9 / p99 32.6 ms; probes beneath: 4,000 checks, 0 missed, commit-lag p95 5.14 s (over the 5 s target)**; **post-D-018 re-run 2026-11-19, release, `078cfed`: 1,000,000/1,000,000 answered 200 (gate passed), 1,569 rps @32 VUs, p50 16.1 / p95 32.4 / p99 42.4 ms; probes beneath: 4,256 checks, 0 missed, commit-lag p95 1.32 s — the over-target lag is gone** |
| Public status read | `perf/k6/public-status.js` (harness `run-public-status.sh`, workflow `load.yml`) | anonymous `GET /s/<slug>/status.json` — 8/10 served beat, 1/10 born-expired, 1/10 missing; 16,384 synthetic XFF visitors model real distribution (the deployment's own trusted-proxy shape), exercising the per-address throttle's eviction path | scenario + harness + dispatch workflow exist; the ≥1M run is dispatched on CI |
| Session churn | `perf/session-churn.mjs` | `POST /api/login` + cookie-authed writes at rate — store-side sessions bounded, sweep keeps up | **post-D-018 re-run 2026-11-19, debug, `078cfed`→`ca8dcd5`** (see below): first run caught 2 login failures (SQLITE_BUSY, `busy_timeout` was 0 → fixed); re-run 306,500 logins 0 fails — PASS |
| Retention under load | retention drill (define) | aggregates roll while checks land — stats union stays honest mid-sweep | partially covered by tests — live drill |
| Backup under load | `scripts/backup-drill.sh` during lab profile | `VACUUM INTO` beside a writing workload — snapshot consistent | drill exists — run under load |
| Restart under load | kill mid-profile → `Health::rebuild` | streaks + open incidents rebuilt from committed evidence | partially covered — rerun |
| **24-hour soak** | sustained lab profile (or real-app) for 24 h | the guide's last gate: no leak, honest coverage for a day | **OWED — `soak-24h-20261106.log` is an empty placeholder; run it and overwrite with real output** |

## Gaps to close this pass

1. Run the 24-hour soak — the only owed item. Replace the empty placeholder with real output.
2. Define the three missing scenarios (incident storm, API reads, session churn) as small
   `.mjs` harnesses beside `lab-profile.mjs` — same lab profile, assert through the real API.
3. Retention + backup live drills under load.

## 2026-11-07 pass

- `perf/storm.mjs` added: incident storm (40 dead monitors across 10 origins) + API reads in one
  run. Validated at 180s: 40/40 incidents opened, survivor kept its 15s cadence (probes never
  starved), 2,878 API reads / 0 failures. Evidence run pending the 24h soak finishing.
- Design note: dead monitors must spread across origins — one origin throttles to the per-origin
  bound and slots go missed (the product's own discipline).
- Session churn remains a gap: needs a non-lab profile with seeded admin + session auth —
  not covered by lab mode's unauthenticated API.

## 2026-11-17 pass (release build, `304edd5`)

**1,000,000 API requests — committed, gate passed.** `perf/api-load.mjs --bin release`
(`perf/results/api-load-2026-11-17T105732331Z-304edd5.json`): 1,000,000 bearer-authed reads
over 32 k6 VUs in 607 s — every one answered 200, so the correct-token refund path never
threw a 429 under concurrency. 1,647 rps; p50 15.7 / p95 30.9 / p99 32.6 / max 51 ms. By
route (p50/p99 ms): list 15.4/18.5, one monitor 15.0/18.1, incidents 14.4/17.6, stats
28.6/33.8, checks 29.6/35.0. The 100-monitor lab mix ran beneath: 4,000 checks in the
window, 0 missed, but commit lag p95 was **5,143 ms, over the 5 s target** (module 09)
that the unloaded baseline met at 4.9 s. That is a miss against the target, kept. Server
RSS 22.2 → 26.5 MB; DB 0.9 MB. Load average 1.70/4.29/4.19 before, 8.79/7.72/6.15 after:
the run itself shares the 12 cores (k6, server, 10 fixture processes) and Chrome
renderers from other work were using about two cores by the end, so the second half was
not quiet.

**≥1,000,000 checks — not run: one server cannot produce them in a session.** The
coordinator dispatches only on its 1 s tick and takes at most `global_slots` (20) probe
permits per tick (`schedule.rs`: `plan_due` runs on `ticker.tick()`, `try_acquire` →
"not this tick"), so one server's ceiling is 20 checks/s even with perfectly spread
phases, and 1,000,000 checks need at least 50,000 s (13.9 h). Monitors created in the
same tick share a phase (no jitter), so a phase-locked wave gets 20 × 5 grace ticks = 100
dispatches per 15 s; the rest are counted missed. The 2026-11-17 ceiling probe confirms
it (`perf/results/2026-11-17T10-55-11.868Z-304edd5.json`: `lab-profile.mjs --bin release
--mix ok --monitors 200 --duration-s 120`): 1,600 slots = 800 pass (100 per wave) + 800
missed, every miss counted; RSS flat at 24 MB. It also explains the 100-monitor baseline's
p95 lag of 4.9 s: that profile sits exactly at the 100-per-wave ceiling. Whether to run
~14 h, add phase jitter, or dispatch on probe completion as well as on the tick is a
maintainer decision, not a harness change.

## 2026-11-19 post-D-018 re-runs (`078cfed`, machine apple-m2-pro-12c, Darwin arm64)

- **Ceiling probe** — `perf/results/2026-11-19T02-39-34.901Z-078cfed.json`:
  `lab-profile.mjs --bin release --mix ok --monitors 200 --duration-s 120 --origins 9`:
  1,574 committed checks (1,570 pass, 4 transport), **0 missed** — vs 800 missed at
  `304edd5`. Commit-lag p50 0.58 / p95 2.51 / max 4.60 s. The phase spread + schedule-sized
  budget closed the 100-per-wave ceiling the 2026-11-17 run proved. (`--origins 9`: one
  band port was occupied; origin count is recorded in the run header.)
- **API read surface** — `perf/results/api-load-2026-11-19T025448868Z-078cfed.json`:
  `--bin release --requests 1000000 --vus 32 --monitors 100 --origins 9`: gate passed —
  1,000,000/1,000,000 answered 200, 1,569 rps, p50 16.1 / p95 32.4 / p99 42.4 ms; probes
  beneath: 4,256 checks, **0 missed**, commit-lag p95 1.32 s (the prior run's 5.14 s
  over-target lag is gone).
- **Storm** — `perf/results/storm-2026-11-19T031356653Z.json`: `--duration-s 300 --origins 8`,
  debug: 40 dead monitors → 40 incidents, the survivor kept its probe slots (20 checks),
  4,582 API reads 0 failures — PASS.
- **Session churn** — `perf/results/session-churn-2026-11-19T033002864Z.json` (re-run) and
  `…032135006Z.json` (the run that caught it): first run at `078cfed` **failed its
  `logins_work` gate — 2 of 310,678 logins and a delete returned SQLITE_BUSY**; pooled
  connections carried SQLite's default `busy_timeout` of 0. Fixed in `ca8dcd5`
  (`busy_timeout` 5 s + regression test); re-run: 306,500 logins, 0 fails, sessions bounded,
  probes unstarved — PASS.

Caveats: runs are development observations on this machine — never public capacity claims.
`sysctl`/`ps` are denied in the run environment, so `loadavg_*`/`rss_kb` fields are null.
The 24 h soak + ≥1M-check run remain droplet work, not local.
