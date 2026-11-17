# Service Monitor load matrix — stability and scale evidence

Goal: committed evidence the monitor tells the truth under load — probes stay bounded, misses
are counted, incidents and alerts stay honest, the API and dashboard reads hold — per the
guide and the workspace ≥1M standard (units: probe checks / API requests where applicable).

Honesty: numbers are development observations on this machine, not production-capacity claims;
saturation shape and failure behavior are the evidence. `SMON_PROFILE=lab` permits the loopback
fixtures; never on a public bind.

| Module | Scenario | What it proves | Evidence status |
|---|---|---|---|
| Probe engine | `perf/lab-profile.mjs` | 100 monitors @ 15s phase-locked, 10 fixture origins — bounded pools, counted misses, commit lag | committed (4000 checks, 0 missed, p95 lag 4.9s); 2026-11-17 ceiling probe (200 `/ok` monitors, release, `304edd5`): 800 pass + 800 missed — see below; **≥1M checks not run**; **fixed in D-018** (phase spread, schedule-sized budget, named refusal): re-run pending a quiet machine |
| End-to-end truth | `perf/real-app.mjs` + `real-app-load.mjs` | real Node app under rising load with `__ctl` degradation — monitor's readings vs app's own ground truth | committed (3.84M requests, 0 missed, incident times honest, both alert channels) |
| Failure/incident storm | mass-failure drill (define) | N targets fail together → N incidents + outbox rows in bounded transactions; delivery pool never starves probes | gap — define + run |
| API read surface | `perf/api-load.mjs` (k6 `perf/k6/api-reads.js`) | `/api/**` reads at sustained rate while probes run — token auth, RLS-free SQLite reads | **committed 2026-11-17, release, `304edd5`: 1,000,000/1,000,000 answered 200 (gate passed), 1,647 rps @32 VUs, p50 15.7 / p95 30.9 / p99 32.6 ms; probes beneath: 4,000 checks, 0 missed, commit-lag p95 5.14 s (over the 5 s target)** |
| Session churn | login-rate scenario (define) | `POST /api/login` + cookie-authed writes at rate — store-side sessions bounded, sweep keeps up | gap — define + run |
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
