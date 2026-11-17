# Module 03 — Coordinator + probes (`crates/monitor-server`, `probe`/`schedule`)

**Status:** built (M1); SSRF complete (M5.2): redirects followed with per-hop revalidation,
DNS answers checked inside the resolver; TLS/certificate classification complete (M6.1):
deterministic webpki-roots trust (D-010) + committed cert detail taxonomy.

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
- **Phase (D-018):** each monitor's slots sit at a fixed phase within its interval, from its id, so monitors created together never fall due on one tick.
- **Concurrency bounds:** global probe semaphore sized to the schedule, `Σ (tick + timeout)/interval`, 20 to 512 (D-018; a schedule past it, or past 2 starts a tick on one origin, is refused at write time with 409 `capacity_exceeded`), per-origin semaphore 2 (keyed by
  `host:port`), one in-flight probe per monitor. Bounds are `try_acquire` — contention means
  `missed`, not a queue.
- **Classification:** `Pass` = acceptable status (2xx in v1; assertions later);
  `HttpFail{status}` = any other status incl. exhausted redirects; `Transport` = connect/DNS/
  TLS/body failure; `Timeout` = deadline exceeded. Never infer. `Transport` carries a stable
  `detail`: `rejected_local` (SSRF), `cert_expired`, `cert_not_yet_valid`,
  `cert_name_mismatch`, `cert_untrusted`, `tls`, `connect`, `body`, `request`, or none.
  Certificate classes map from rustls `CertificateError` under webpki-roots trust (D-010);
  details are fixed strings — no cert contents, URLs, or raw error text are committed.
- **Generation invalidation:** probe carries `generation`; the writer drops results whose
  generation no longer matches the stored monitor.
- **Target policy (SSRF, complete in M5.2):** two layers, both live. URL layer — scheme
  `http`/`https` only (checked at probe time and at every redirect hop); host as an IP
  literal: every non-global block of the IANA IPv4 and IPv6 special-purpose registries
  rejected unless `SMON_PROFILE=lab` — 0/8, 10/8, 100.64/10 (CGNAT), 127/8,
  169.254/16, 172.16/12, 192.0.0/24, the three TEST-NETs, 192.88.99/24, 192.168/16,
  198.18/15, 224/4, 240/4 (incl. broadcast); `::/96`, 64:ff9b:1::/48, 100::/64, 100:0:0:1::/64 (RFC 9780),
  2001::/23, 2001:db8::/32, 3fff::/20, 5f00::/16, fc00::/7, fe80::/10, fec0::/10,
  ff00::/8 — and v4-mapped, NAT64 (64:ff9b::/96) and 6to4 (2002::/16) addresses are
  judged by the IPv4 address they embed (`LOCAL_V4`/`LOCAL_V6` in `probe.rs`). DNS layer — `PolicyResolver` validates every address the OS answer
  contains *inside* reqwest's resolver, so the dialer only ever sees validated addresses
  (no validate-then-re-resolve gap); one local address fails the whole resolution —
  round-robin DNS makes "usually public" not a defense. Redirects: followed up to 5 hops,
  each hop revalidated through the same URL check; a refused hop leaves the 3xx as
  `http_fail` evidence. A DNS refusal classifies `rejected_local` (cause-chain walk) and commits as a `transport` failure that pages (D-013), so
  it never masquerades as a plain `connect` error. The probe client ignores
  `HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY` (`no_proxy`): a proxied request is resolved by
  the proxy and would bypass the DNS layer (`proxy_environment_variables_route_nothing`).

## Failure cases

| Trigger | Required result |
|---|---|
| Target hangs | `timeout` at deadline; worker freed |
| Workers saturated / probe in flight | slot `missed`, counted, `unknown` semantics apply |
| Edit/pause mid-probe | generation bump; late result dropped at writer |
| Redirect to refused target | hop stopped; the 3xx stands as `http_fail` |
| Redirect loop past 5 hops | last 3xx stands as `http_fail` |
| Name resolves to a local address | DNS-layer refusal → `transport` + `rejected_local`, a failure that pages (D-013; `a_name_that_resolves_to_a_local_address_is_a_failure_that_pages`) |
| Redirect to a local *name* | the resolver refuses the hop → `transport` + `rejected_local`, pages (`a_redirect_to_a_local_name_is_a_failure_that_pages`) |
| Local/private literal registered or edited in (strict profile) | 422 `refused_target` at the API — never stored (`refused_targets_cannot_be_registered_outside_the_lab_profile`) |
| Expired / not-yet-valid cert | `transport` + `cert_expired` / `cert_not_yet_valid` |
| Trusted cert, wrong hostname | `transport` + `cert_name_mismatch` |
| Self-signed / untrusted chain | `transport` + `cert_untrusted` |
| Other TLS handshake failure | `transport` + `tls` |

## Testing parameters

- **Unit:** classify each reqwest error kind; policy accept/reject matrix (schemes, IP
  literals, lab vs normal profile); `is_local_matches_the_ipv4_special_purpose_registry` and `…_ipv6_…` —
  first/last address of every refused block, the public neighbours just outside, and
  the embedded-IPv4 forms; `validate_addrs` mixed-answer refusal; the resolver refusing a real
  `localhost` lookup under the strict profile.
- **Integration (ephemeral fixtures):** 200 → `pass`; 500 → `http_fail`; silent socket →
  `timeout` under a 200 ms test deadline; refused port → `transport`; chunked 1 MiB body →
  pass with ≤64 KiB read.
- **TLS fixtures (rcgen + tokio-rustls, real handshakes):** expired leaf → `cert_expired`;
  not-yet-valid → `cert_not_yet_valid`; self-signed → `cert_untrusted`; CA-signed leaf with
  wrong SAN trusted via `extra_roots` → `cert_name_mismatch`; CA-signed correct leaf →
  `pass`. Live drill: `expired.badssl.com` → `cert_expired`, `wrong.host.badssl.com` →
  `cert_name_mismatch` (committed rows, 2026-11-06).
- **Scheduler test (paused clock / short intervals):** 50 ms interval monitor produces
  ordered slots; two monitors never probe the same origin >2 concurrently; a 200 ms-hung
  target at 100 ms interval yields `missed` slots, never overlap.
- **Load (module 09):** p95 dispatch lag < 5 s under the 100-monitor lab profile.
