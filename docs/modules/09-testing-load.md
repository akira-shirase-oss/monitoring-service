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

## Load plan (guide lab profile — proposed targets, not measured)

- **Shape:** 100 monitors @ 60 s, 20 global slots, 2 per origin; healthy/slow/failing mix;
  24 h; plus a 5× short burst.
- **Targets:** p95 dispatch lag < 5 s; no unexplained steady memory growth; missed checks
  counted and reported, never hidden; storage inside the ~2 GiB guardrail.
- **Comparisons:** identical fixtures, recorded commit + machine; laptop numbers are
  development observations, never public claims.
- **Scenario files:** `perf/k6/` as they land; results committed to `perf/results/` with
  machine and commit (module 05/02/03 assertions fold into the soak).

## Testing parameters for this module itself

- Fixtures start deterministically and reset state; each fault test asserts the *monitor's*
  required result, not the fixture's cleverness.
