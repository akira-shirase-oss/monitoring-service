# Changelog

## Unreleased

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
