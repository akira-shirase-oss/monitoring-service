# Module 03 — Coordinator + probes (`crates/monitor-server`, `probe`/`schedule`)

**Status:** built (M1). SSRF staging: URL-layer policy (scheme + IP literals) now; redirect
following and DNS-resolution checks in M5.

## Purpose

Fire one safe `GET`/`HEAD` attempt per scheduled slot, classify the result honestly, and hand
it to the writer — without ever letting a slow target stall the schedule.

## Contract

- **Slot model:** per monitor, `next_slot` advances by `interval` from the last due time. One
  attempt per slot: if a slot can't be dispatched before the next one is due (or the monitor
  already has a probe in flight), the slot is `missed` and counted — never queued as a storm.
- **Probe bounds:** total deadline 5 s (per-monitor `timeout_s`, whole attempt including DNS,
  connect, TLS, headers and the bounded body read); response body capped at 64 KiB then the
  connection is dropped; `GET` only in v1 (`HEAD` opt-in later).
- **Concurrency bounds:** global probe semaphore 20, per-origin semaphore 2 (keyed by
  `host:port`), one in-flight probe per monitor. Bounds are `try_acquire` — contention means
  `missed`, not a queue.
- **Classification:** `Pass` = acceptable status (2xx in v1; assertions later);
  `HttpFail{status}` = any other status incl. exhausted redirects; `Transport` = connect/DNS/
  TLS/body failure; `Timeout` = deadline exceeded. Never infer.
- **Generation invalidation:** probe carries `generation`; the writer drops results whose
  generation no longer matches the stored monitor.
- **Target policy (SSRF, M1 minimal → M5 full):** scheme `http`/`https` only (validated at
  registration); no redirect following in v1 (a redirect is `HttpFail{3xx}`); host as an IP
  literal: loopback/private/link-local rejected unless `--profile lab`. Deferred to M5:
  redirect-following with per-hop revalidation and DNS-resolution IP checks (rebinding).

## Failure cases

| Trigger | Required result |
|---|---|
| Target hangs | `timeout` at deadline; worker freed |
| Workers saturated / probe in flight | slot `missed`, counted, `unknown` semantics apply |
| Edit/pause mid-probe | generation bump; late result dropped at writer |
| Redirect | `http_fail` with the 3xx status (v1) |

## Testing parameters

- **Unit:** classify each reqwest error kind; policy accept/reject matrix (schemes, IP
  literals, lab vs normal profile).
- **Integration (ephemeral fixtures):** 200 → `pass`; 500 → `http_fail`; silent socket →
  `timeout` under a 200 ms test deadline; refused port → `transport`; chunked 1 MiB body →
  pass with ≤64 KiB read.
- **Scheduler test (paused clock / short intervals):** 50 ms interval monitor produces
  ordered slots; two monitors never probe the same origin >2 concurrently; a 200 ms-hung
  target at 100 ms interval yields `missed` slots, never overlap.
- **Load (module 09):** p95 dispatch lag < 5 s under the 100-monitor lab profile.
