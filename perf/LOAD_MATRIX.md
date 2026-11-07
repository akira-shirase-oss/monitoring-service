# Service Monitor load matrix — stability and scale evidence

Goal: committed evidence the monitor tells the truth under load — probes stay bounded, misses
are counted, incidents and alerts stay honest, the API and dashboard reads hold — per the
guide and the workspace ≥1M standard (units: probe checks / API requests where applicable).

Honesty: numbers are development observations on this machine, not production-capacity claims;
saturation shape and failure behavior are the evidence. `SMON_PROFILE=lab` permits the loopback
fixtures; never on a public bind.

| Module | Scenario | What it proves | Evidence status |
|---|---|---|---|
| Probe engine | `perf/lab-profile.mjs` | 100 monitors @ 15s phase-locked, 10 fixture origins — bounded pools, counted misses, commit lag | committed (4000 checks, 0 missed, p95 lag 4.9s) |
| End-to-end truth | `perf/real-app.mjs` + `real-app-load.mjs` | real Node app under rising load with `__ctl` degradation — monitor's readings vs app's own ground truth | committed (3.84M requests, 0 missed, incident times honest, both alert channels) |
| Failure/incident storm | mass-failure drill (define) | N targets fail together → N incidents + outbox rows in bounded transactions; delivery pool never starves probes | gap — define + run |
| API read surface | dashboard-poll scenario (define) | `/api/**` reads at sustained rate while probes run — token auth, RLS-free SQLite reads | gap — define + run |
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
