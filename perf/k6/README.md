# perf/k6/

Load scenarios with thresholds (the guide's lab profile: 100 monitors at 60-second intervals,
20 global slots, two per origin, over 24 hours; target p95 dispatch lag under 5 seconds, no
unexplained memory growth). Numbers here are targets until a run produces evidence.

Nothing here yet — scenarios arrive with the coordinator. Results go in `perf/results/` with
the machine and commit recorded.
