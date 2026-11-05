# Changelog

## Unreleased

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
