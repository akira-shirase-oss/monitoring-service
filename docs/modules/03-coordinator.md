# Module 03 — Coordinator + probes (`crates/monitor-server`, `probe`/`schedule`)

**Status:** built (M1); SSRF complete (M5.2): redirects followed with per-hop revalidation,
DNS answers checked inside the resolver.

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
- **Target policy (SSRF, complete in M5.2):** two layers, both live. URL layer — scheme
  `http`/`https` only (checked at probe time and at every redirect hop); host as an IP
  literal: loopback/private/link-local/unspecified/broadcast/multicast rejected unless
  `SMON_PROFILE=lab`. DNS layer — `PolicyResolver` validates every address the OS answer
  contains *inside* reqwest's resolver, so the dialer only ever sees validated addresses
  (no validate-then-re-resolve gap); one local address fails the whole resolution —
  round-robin DNS makes "usually public" not a defense. Redirects: followed up to 5 hops,
  each hop revalidated through the same URL check; a refused hop leaves the 3xx as
  `http_fail` evidence. A DNS refusal classifies `rejected_local` (cause-chain walk), so
  it never masquerades as a plain `connect` error.

## Failure cases

| Trigger | Required result |
|---|---|
| Target hangs | `timeout` at deadline; worker freed |
| Workers saturated / probe in flight | slot `missed`, counted, `unknown` semantics apply |
| Edit/pause mid-probe | generation bump; late result dropped at writer |
| Redirect to refused target | hop stopped; the 3xx stands as `http_fail` |
| Redirect loop past 5 hops | last 3xx stands as `http_fail` |
| Name resolves to a local address | `transport` + `rejected_local` (DNS-layer refusal) |

## Testing parameters

- **Unit:** classify each reqwest error kind; policy accept/reject matrix (schemes, IP
  literals, lab vs normal profile); `is_local` over every non-public range incl.
  v4-mapped v6; `validate_addrs` mixed-answer refusal; the resolver refusing a real
  `localhost` lookup under the strict profile.
- **Integration (ephemeral fixtures):** 200 → `pass`; 500 → `http_fail`; silent socket →
  `timeout` under a 200 ms test deadline; refused port → `transport`; chunked 1 MiB body →
  pass with ≤64 KiB read.
- **Scheduler test (paused clock / short intervals):** 50 ms interval monitor produces
  ordered slots; two monitors never probe the same origin >2 concurrently; a 200 ms-hung
  target at 100 ms interval yields `missed` slots, never overlap.
- **Load (module 09):** p95 dispatch lag < 5 s under the 100-monitor lab profile.
