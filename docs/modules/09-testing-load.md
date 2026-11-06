# Module 09 — Fixtures, faults and load (`fixtures/`, `perf/`)

**Status:** `fixtures/fault-server.mjs` live (M2): ok/fail/slow/hang/status/flaky + `/sink`
webhook receiver for delivery proofs. Load runs remain M5.

## Purpose

Proof infrastructure: deterministic targets that fail on command, process-level fault
injection, and load runs whose numbers mean something. The old fixtures are gone (D-001) —
these are rebuilt from the contract.

## Fixture matrix (`fixtures/fault-server`, Node)

| Endpoint | Behavior |
|---|---|
| `/ok` | 200 fast |
| `/flaky` | configurable sequence (e.g. fail×3, ok×2) — drives open/recover cycles |
| `/slow?ms=` | delayed 200 (probe-deadline tests) |
| `/hang` | accepts, never responds (deadline/delivery-pool isolation) |
| `/status/{code}` | arbitrary status, incl. 3xx (redirect policy) |
| `/big?bytes=` | oversized body (64 KiB bound) |
| `/record` | webhook receiver: records deliveries for outbox tests |

Each run is seeded/configured so fault tests are reproducible, per the portfolio rule that a
failure demo must be repeatable.

## Fault tests

- **Process-kill:** SIGKILL between probe and commit → coverage gap, no fabricated outcome;
  between commit and delivery → event delivered on restart.
- **Disk:** read-only/full data dir → `/readyz` 503, no authoritative updates.
- **Monitor death:** heartbeat expiry → "monitoring unavailable" (module 07).

## Load plan — first measured baseline (M5.3)

- **Harness:** `perf/lab-profile.mjs`. The monitor generates its own load (an external
  driver would only exercise the API), so the harness orchestrates: N fixture origins,
  one monitor-server in `lab` profile, 100 monitors registered inside ~130 ms — all at
  the minimum 15 s interval so every wave is a phase-locked 100-slot burst on the 20
  global slots (the "5× burst" is built in, every 15 s, for the whole run).
- **Mix:** 50 `/ok`, 20 `/flaky` (varied patterns so phases decorrelate), 15 `/slow` (2 s
  inside a 3 s deadline), 10 `/status/500` (permanent), 5 `/hang` (timeout every slot).
- **Measured** (`perf/results/2026-11-06T01-18-54.904Z-33656f5.json`, Darwin arm64,
  10 min): 4000 checks = 100 × 40 slots exactly — zero missed, zero extras; 22.5%
  non-pass rate held (699 http_fail + exactly-200 timeouts = all hung slots); commit lag
  (recorded_at − slot_at − probe latency, an upper bound on dispatch lag) p50 2.9 s /
  p95 4.9 s — under the 5 s grace but with ~2% headroom, the wave-drain queue is the
  whole story; 62 incidents → 107 outbox events = 45 resolved×2 + 17 open, all 107
  delivered on attempt 1, sink hits match exactly (no dupes); RSS 27→21 MB flat; DB
  840 KB.
- **Targets:** p95 dispatch lag < 5 s; no unexplained steady memory growth; missed checks
  counted and reported, never hidden; storage inside the ~2 GiB guardrail.
- **Not yet run:** the 24 h soak — the bounded run above is the baseline; a long soak is
  the remaining evidence, and nothing here claims it.
- **Comparisons:** identical fixtures, recorded commit + machine; laptop numbers are
  development observations, never public claims.

## Real-app rising-load drill (M7.3)

The lab profile proves the monitor keeps bounded evidence under its own generated load;
it does **not** prove the readings reflect a real application. The real-app drill closes
that gap: `perf/real-app.mjs` is a dependency-free Node app that does genuine work per
request (`GET /api/items` scans a filtered row set and aggregates; `POST /api/echo`
round-trips a bounded body; `GET /health` is the control target), records its own
ground-truth route/error/latency stats at `GET /__stats`, and takes a degradation
fraction through `POST /__ctl` — failures happen *after* the real work runs, so the
monitor sees honest slow failures, not refused connections.

`perf/real-app-load.mjs` orchestrates: a closed-loop load generator ramps 2 → 40
concurrent workers (~7.5 min), the monitor watches `/api/items` (15 s interval,
fail-open 2, pass-recover 1) and `/health` (the control — never paged), the app is
degraded at 85% for the bulk of the window then 100% for the last 45 s (so a stable-open
incident always reaches the recovery phase — at 85%, a single lucky pass resolves the
incident mid-window, which is the domain being honest, not a flake), then heals.
Everything the drill asserts is gathered through the real API: committed check rows,
incidents, webhook events at a real sink, and the app's own `__stats` as ground truth.

- **Measured** (`perf/results/real-app-2026-11-06T14-03-29.168Z-825110a.json`,
  Darwin arm64, 440 s): 3,837,352 requests sent, 0 client errors, client p95 7 ms;
  app truth on `/api/items` — 2,687,372 requests, 777,060 errors (28.9% across the full
  run, concentrated in the degrade window as designed). Monitor: 30+30 committed rows,
  **0 missed**, commit lag p50 787 ms / p95 842 ms, RSS flat at ~18 MB. One incident:
  `first_fail_at` 14:07:00 → `opened_at` 14:07:15 (2-fail threshold) → `last_fail_at`
  14:08:45 → `resolved_at` 14:09:00 (first pass after heal) — all four times honest.
  Both alerts delivered (open + recovered, 1 event each at the sink). The `/health`
  control monitor committed 30 checks and never paged. Probe-observed ramp p95 latency
  61 ms while the app served ~9k req/s.
- **Assertions (12/12):** real traffic committed; evidence ≈ expected slots; zero
  missed; degrade window read as `http_fail`; incident opened in-window; **every**
  incident healed once the app did; control quiet; open + recover alerts delivered; app
  recorded the outage in its own stats; probe latency bounded under load; commit lag
  p95 < 15 s.
- **Rule:** harness failures are evidence — a failed assertion documents a real behavior
  difference (the mid-window-recovery semantics above were found this way), not a reason
  to rerun until green.

## Testing parameters for this module itself

- Fixtures start deterministically and reset state; each fault test asserts the *monitor's*
  required result, not the fixture's cleverness.
