# perf/

Load evidence (module 09). The monitor is its own load generator — external drivers like
k6 would only exercise the management API — so the harness is `perf/lab-profile.mjs`:
it spawns N fault-fixture origins plus a `lab`-profile monitor-server, registers 100
monitors in one wave (a phase-locked 100-slot burst every 15 s — the required "burst"
is inherent), samples RSS and outbox depth, then writes a results JSON here.

    node perf/lab-profile.mjs --duration-s 600 --monitors 100 [--origins 10]

Baseline (M5.3, commit 33656f5, Darwin arm64, 10 min): 4000/4000 slots accounted —
zero missed; commit-lag p95 4.9 s vs the 5 s target; 107/107 webhook events delivered
first-attempt; RSS flat. Laptop numbers are development observations, never public
capacity claims. The 24 h soak from the guide's lab profile is not yet run — the
baseline is what exists.
