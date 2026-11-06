# Changelog

## Unreleased

- M6.1 TLS/certificate classification: `transport` checks now carry a stable `detail`
  drawn from rustls `CertificateError` — `cert_expired`, `cert_not_yet_valid`,
  `cert_name_mismatch`, `cert_untrusted`, `tls` — instead of flattening to `connect`.
  The client builds its own `ClientConfig` on `webpki-roots` (D-010): identical trust
  on every OS, no silent reliance on the operator's keychain; internal PKI opts in
  through `ProbePolicy.extra_roots`. Committed details are fixed strings — never cert
  contents, URLs, or raw error text. Proven with real rcgen/tokio-rustls handshakes
  (5 fixtures) and live against `expired.badssl.com` → `cert_expired`,
  `wrong.host.badssl.com` → `cert_name_mismatch`.
- M5.3 load baseline: `perf/lab-profile.mjs` runs the monitor's own scheduler as the
  load generator — 100 monitors at the minimum 15 s interval registered in one wave,
  so every wave is a phase-locked burst on the 20 global slots across 10 fixture
  origins (the mix: 50 ok / 20 flaky / 15 slow / 10 dead / 5 hung). Measured on
  Darwin arm64 for 10 min: 4000/4000 slots accounted (zero missed), commit-lag p95
  4.9 s vs the 5 s grace, 62 incidents → 107 outbox events all delivered
  first-attempt (sink hits match exactly), RSS flat at ~27 MB, DB 840 KB. The guide's
  24 h soak is documented as not yet run — the bounded baseline is the evidence that
  exists. `GET /version` now reports the binary's own version for upgrade checks.
- M5.2 SSRF complete: `PolicyResolver` runs inside reqwest's DNS resolution — every
  address in the OS answer must be public (unless `SMON_PROFILE=lab`), and reqwest only
  ever dials the validated set, so there is no validate-then-re-resolve gap; one local
  address fails the whole answer. Redirects are now followed (≤5 hops) with every hop
  revalidated through the same URL-layer policy — a refused or looping hop leaves the
  last 3xx as `http_fail` evidence. DNS-layer refusal classifies `rejected_local` via a
  cause-chain walk instead of an ambiguous `connect`.
- M5.1 management auth: `SMON_ADMIN_TOKEN` requires `Authorization: Bearer` on every
  `/api/**` route, compared on SHA-256 digests (fixed-length, no timing leak). Unset is
  dev mode — allowed on loopback with a loud warning, refused on any public bind before
  a single task spawns. `/livez`, `/readyz` and static files stay open; the dashboard
  shows a token gate on 401 (stored in localStorage), and the CLI sends the token from
  `--token`/`SMON_ADMIN_TOKEN`.
- M4.4 deployment: `scripts/deploy.sh` installs versioned releases
  (`releases/<rel>/bin` + `current` symlink, data outside releases so a rollback can't
  roll back evidence), with a systemd unit, a launchd plist, and `deploy/RUNBOOK.md`
  covering install/upgrade/rollback/backup/heartbeat-watching. Exercised end to end:
  incident open on v1 → upgrade to v2 → still `down` → rollback to v1 → still `down`.
- M4.3 backup + restore: `Db::backup` runs `VACUUM INTO` (a WAL-consistent online
  snapshot — reads the store, never writes it) behind `POST /api/backup`, which requires
  an absolute path and reports the snapshot's byte size. `scripts/backup-drill.sh`
  rehearses the whole drill: open an incident, back up, destroy, restore, verify the
  incident and pending outbox are still there.
- M4.2 disk guardrails: `SMON_MAX_DB_BYTES` (default 2 GiB) caps the database via
  `PRAGMA max_page_count` at connect — writes past the ceiling fail `SQLITE_FULL`, never
  corrupt. The writer counts consecutive commit failures in `WriteHealth`; `/readyz`
  answers a real 503 `unavailable` while it is nonzero and clears only when a write lands
  (recovery proven by a commit, not a timer). `SMON_LOG_FILE`/`SMON_LOG_MAX_BYTES` add a
  byte-bounded rotating log (two generations) so our own output honors the same guardrail.
- M4.1 retention: checks older than `SMON_RETENTION_S` (default 7 d) roll into
  `check_stats_daily` (per-day outcome counts, migration 0002) and delete in the same
  transaction — one commit, so a crash can't leave history that is neither raw nor summed.
  The pass runs through the single writer every `SMON_RETENTION_INTERVAL_S` (default 1 h),
  spares every raw row belonging to a monitor with an open incident, never touches the
  outbox, and `check_stats` unions both tables so long-window coverage stays honest.
- M3.4 browser suite: `apps/dashboard/tests/dashboard.spec.ts` driven by `scripts/e2e.sh` —
  Playwright runs the real `monitor-server` (serving the built dashboard on its own origin)
  plus the fault fixture on e2e-only ports. Specs: aborted API renders "cannot see" with no
  fabricated rows, a registered monitor's badge turns `up` on committed evidence, a real
  3-failure sequence opens an incident the detail page shows (checks strip, "still open",
  coverage-vs-success line), and keyboard navigation reaches the detail route.
- M3.2/M3.3 public status + heartbeat: `monitor-server` rewrites `status.json` every
  `SMON_STATUS_INTERVAL_S` (default 30 s) into `SMON_STATUS_DIR` — allowlisted fields only
  (sha256 service alias, status, `incident_open_since`; hashing because UUIDv7 prefixes
  collide for same-millisecond ids), written atomically, self-expiring via
  `SMON_STATUS_TTL_S` (default 90 s). `apps/status/index.html` is the static renderer —
  copy the two files to any host; `fixtures/status-check.mjs` is the independent checker.
  An expired or missing snapshot reports "monitoring unavailable", never "targets down".
  Verified live: beat → alive, kill → expired beat → unavailable, restart → alive.
- M3.1 coverage vs success: `GET /api/monitors/{id}/stats` answers "did we look" and
  "what did we see" as separate numbers — `expected` slots from interval over elapsed wall
  clock (bounded `?window_s=`), `observed`/`missed`/`passed` from stored rows, ratios in
  basis points with `null` for undefined denominators. A monitor born seconds ago reports
  `coverage_bps: null`, not 100%; a window of misses is 0% coverage with undefined success,
  never "down by silence". The detail page renders both.
- M2 "notify": the alert path is real. Outbox deliverer with bounded concurrency,
  deterministic FNV-seeded jitter, per-attempt deadline, HMAC-SHA256 `x-smon-signature` and
  stable `x-smon-event` identity — delivery bookkeeping still goes through the single
  writer (`mark_delivery`); exhausted events stay visible in `notify_outbox`. Management
  writes (`PATCH`, pause/unpause) revalidate the merged monitor and bump `generation`
  atomically (D-009), so in-flight probes die as stale; `DELETE` is a single conditional
  statement that refuses once evidence exists (D-008). New routes `GET
  /api/monitors/{id}/checks` and `GET /api/incidents`, dashboard detail page, CLI
  `show/edit/pause/unpause/delete`, and `fixtures/fault-server.mjs` (ok/fail/slow/hang/
  status/flaky + `/sink` receiver). Verified live (incident→webhook, recovery→webhook,
  hang isolation) and by 61 tests.
- M1 "check and remember": the observing loop is real. Coordinator with one attempt per
  slot, `try_acquire` global/per-origin bounds and counted `missed` rows (D-007); reqwest
  probe engine with a total deadline, a 64 KiB body bound, classified outcomes and
  URL-layer target policy; single-writer task committing check + incident transition +
  outbox in one transaction; `Health::rebuild` recovering streaks and open incidents on
  restart; API statuses that only reflect committed evidence. Verified live end to end
  (probe → incident → restart → recovery) and by 31 tests.
- Planning docs: `END_GOAL`, `ARCHITECTURE`, `ROADMAP` (M0–M5) and `docs/modules/01-09`.
- Scaffold: Cargo workspace (`monitor-domain`, `monitor-db`, `monitor-server`, `monitor-cli`),
  SQLite migration 0001, incident state machine with tests, management API, `monitor` CLI,
  and the SvelteKit + Tailwind v4 + shadcn-svelte dashboard shell.
