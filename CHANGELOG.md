# Changelog

## Unreleased

- M7.2 dashboard login UX: `POST /api/login` exchanges `SMON_ADMIN_TOKEN` for a
  `sessions` row (migration 0007) + `smon_session` cookie — HttpOnly,
  SameSite=Strict, `SMON_SESSION_TTL_S` (default 12 h). Store-side `expires_at` is
  the authority; `POST /api/logout` deletes the row and expires the cookie
  (idempotent). CSRF contract: `Origin` must match `Host` for any credential and is
  mandatory on cookie-authed writes; bearer needs none (CLI unchanged). A wrong
  token gets a fixed delay + 401; a session-store failure answers 503 rather than
  a false 401. Dashboard: `TokenGate`→`LoginGate` (real POST + server-verbatim
  errors), nav Sign out, `apiFetch` carries the cookie — no token in browser
  storage. Tests: store lifecycle, 7 API session tests (cookie flags, Origin
  matrix, logout, ttl=0 expiry), Playwright sign-in spec — all 16 e2e specs now
  run the real auth path. Live drill: full 401→204+cookie→403/403/201→logout→401
  matrix on the wire.
- M7.1 email alerts: `notify_outbox.channel` (migration 0006) — one incident
  transition enqueues one row per configured channel (webhook always; email when
  `SMON_SMTP_DSN`/`SMON_SMTP_FROM`/`SMON_SMTP_TO` are all set). `Sink::Email` is a
  lettre `AsyncSmtpTransport` on rustls+tokio (`aws_lc_rs`, webpki-roots) driving the
  same bounded pool, per-attempt deadline, deterministic backoff and writer
  bookkeeping as webhooks — a dead SMTP relay can never stall webhook delivery.
  Partial SMTP config is a boot error; subjects are CR/LF-sanitized; `Message-ID`
  carries the event id so SMTP retries thread one logical message. Store test: one
  row per channel; integration: a loopback SMTP stub proves deliver+mark,
  550-refusal→retry→deliver (attempts=2), and channel isolation.
- M6.11 maintenance windows: schedule-scoped suppression (`maintenance_windows`,
  migration 0005) read inside the observation gate — a `failures_to_open` crossing in
  an active window commits the check row but yields `IncidentSuppressed` (no incident,
  no outbox). The streak keeps counting, so the first post-expiry failure opens the
  incident immediately; `first_fail_at` honestly lands inside the window. Recovery of
  a pre-existing incident is never suppressed. API `GET|POST
  /api/monitors/{id}/maintenance` + `DELETE /api/maintenance/{id}`; CLI `monitor
  maintenance add|list|rm` with `--from now`. Live drill: 4 failing checks suppressed,
  window deleted → incident opened on the next slot.
- M6.10 mutation testing: `cargo mutants -p monitor-domain` — 48 mutants,
  42 caught, 6 unviable (`Default::default()` on non-Default types), **0 missed**
  after adding three boundary tests the first run exposed as survivors:
  `interval == MIN_INTERVAL` accepted, `timeout == interval` accepted, and
  `rebuild_tail` returning the larger threshold. Evidence committed to
  `perf/results/`.

- M6.9 CLI exit-code contract: the `monitor` binary now distinguishes failure classes a
  script can branch on — `0` success, `1` API/application error (envelope errors),
  `2` usage error (clap rejections and an empty `monitors edit`), `3` server
  unreachable (no HTTP answer), `4` unauthorized (401/403). Errors go to stderr,
  results to stdout. Pinned by `crates/monitor-cli/tests/exit_codes.rs`, which runs
  the real binary against a stub std HTTP server for all seven cases.
- M6.8 guide page: `/guide` renders one glossary (`lib/vocabulary.ts`) — monitor
  statuses, check outcomes, the transport detail taxonomy, incident fields and states,
  plus concepts (coverage vs success, generation, "cannot see", "monitoring
  unavailable", the strip's gap cells, thresholds). Drift-checked both directions by
  `tests/guide_drift.rs`: every serialized `Status`, every `SlotObservation` outcome
  through the store's own `encode_slot`, the detail taxonomy, and the incident words
  must be in the guide — and no guide word may exist that the API can't emit. A new
  enum variant or word without a guide entry now fails the build.
- M6.7 public status view in-app: `monitor-server` now serves the sanitized snapshot
  unmodified at `GET /status.json` — outside `/api`, no token, so public viewers never
  touch the management surface. The dashboard gains `/status`: service rows with hashed
  aliases + status badges + `incident since` times and a "snapshot written Xs ago" meta,
  polled on the writer cadence. A stale `expires_at` reads "Monitoring unavailable —
  this snapshot is stale" with rows kept as stale evidence; a missing snapshot is the
  same verdict with no rows — never "all targets down" and never fabricated green.
  Playwright: live-snapshot, stale-doctor and 404 specs.
- M6.6 dashboard auto-refresh: both pages call `invalidateAll()` on the 15 s minimum
  interval tick — SvelteKit keeps the previous data until the reload resolves, so the
  swap is atomic and a commit mid-poll flips a badge exactly once; an API outage
  mid-poll resolves to "cannot see" through the same load path; hidden tabs skip the
  tick. Playwright: a new monitor surfaces on the next tick without reload/navigation,
  and a route-abort mid-poll yields the unreachable banner (both specs ≈15.6 s, one
  tick).
- M6.5 dashboard uptime strip: `CheckStrip` above the detail-page check rows — one cell
  per committed check (live/attention/draft status tokens, tooltips carry the outcome,
  HTTP status and latency), plus dashed `missing` gap cells where the slot grid shows
  unobserved time (`buildStripCells` measures whole-interval distance, 1.5× tolerance for
  commit lag; a long hole compresses to chunked cells with the true slot count in the
  tooltip). A hole in evidence renders as a hole, never interpolated green. Playwright:
  pass/http_fail cells on the up/down specs; the pause-gap spec pauses a monitor for two
  slots, resumes, and asserts the `data-outcome="missing"` cell — real unobserved time,
  not a mock.
- M6.4 dashboard mutations: the UI now holds write authority behind the existing token
  — "Add monitor" on the list page (shared `MonitorForm` component), and pause/resume,
  edit (interval/timeout/thresholds sent with the `generation` optimistic-concurrency
  guard), and delete behind a two-click confirm on the detail page. Server envelope
  errors are shown verbatim: a stale-generation `conflict`, the delete-with-history
  refusal ("pause it instead"), and a 401 token hint — no silent failures. Incident rows
  now show `first_fail_at` ("failing since") alongside opened/resolved (M6.3 surface).
  Playwright: full mutation spec (create → pause API-verified → edit, generation 1→2→3
  per write → delete), history-refusal message, and a mutation-401 spec.
- M6.3 incident four times: `first_fail_at`, `opened_at`, `last_fail_at`, `resolved_at`
  are now committed separately (migration 0004). `first_fail_at` is derived inside the
  commit transaction from committed check rows — the oldest consecutive failure since
  the last `pass`/`missed` — so it survives restarts without trusting memory; the
  writer passes the post-transition open flag so the streak lookup runs on the same
  read. `last_fail_at` re-stamps on every failing check while the incident is open.
  All four are exposed in `GET /api/incidents`, the webhook `incident` reference, and
  `monitor … show`; pre-M6.3 rows keep NULL rather than fabricated history. Live proof:
  a refused-port monitor shows `first_fail_at` 30 s before `opened_at`, `last_fail_at`
  still advancing while open.
- M6.2 per-monitor incident thresholds: `failures_to_open` / `passes_to_recover`
  (`1..=10`, default 3/2) on `POST`/`PATCH /api/monitors` and `monitor add|edit`.
  `Health::record` takes thresholds per call from `monitor_gate` — the same committed
  row read as the generation — so a threshold edit takes effect atomically with the
  generation bump that invalidates in-flight probes; no stale-config streaks. Boot
  rebuild replays under current thresholds (`Thresholds::rebuild_tail` sizes the tail).
  Live proof: `failures_to_open=1` monitor opened its incident on the first check while
  a default monitor waited for three.
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
