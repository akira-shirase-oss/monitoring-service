# Decisions

Newest first. `D-NNN` ids are permanent; referenced from code and build-log entries.

## D-007 — Missed slots are stored `checks` rows, not gaps

The scaffold's schema comment said a missed slot would be "visible by the absence of a row."
That model breaks restart rebuild: the rebuild replays stored outcomes, and two failures
separated by an unstored miss would replay as consecutive — fabricating an incident that never
opened. Every slot now produces a row (`outcome = 'missed'` for slots that never ran), so
coverage, streak rebuild and replay all read the same history. Coverage % = non-missed /
total rows.

## D-006 — Ports: 58400 for the server, 58600 for the dashboard's dev server

`SMON_LISTEN` defaults to `127.0.0.1:58400`; vite dev runs on `127.0.0.1:58600` and proxies
`/api`, `/livez`, `/readyz`. Picked adjacent to Colophon's 58000/58200 block without colliding.

## D-005 — Provisional status semantics (the `Health` state machine)

Implemented in `monitor-domain`: three consecutive failures open an incident, two fresh passes
confirm recovery, a missed slot resets both streaks and returns the display to `unknown`, and an
open incident survives restarts until evidence resolves it. Sharpen this when the coordinator
lands — in particular whether an open incident during coverage gaps displays `down` or a
separate "last known down, monitoring stale" state.

## D-004 — shadcn-svelte components live inside `apps/dashboard`

No `packages/ui` yet: with one app, a shared package is indirection without a customer. When the
independently hosted public status renderer arrives, extract shared pieces then — not before.
`components.json` is configured so `npx shadcn-svelte add <component>` drops new components in
`src/lib/components/ui`.

## D-003 — SQLite via sqlx, WAL on, foreign keys on; pool now, dedicated writer later

`monitor-db` opens the file with `create_if_missing`, `journal_mode=WAL`, `foreign_keys=ON`.
The guide calls for a dedicated database worker so writes serialize and never stall probe
workers; that task boundary arrives with the coordinator. Until then handlers use the pool
directly — bounded at 4 connections.

## D-002 — Layout and conventions mirror Colophon

`crates/` (domain, db, server, cli), `apps/`, `packages/`, `scripts/`, `docs/`, `fixtures/`,
`perf/`; same toolchain pin (1.99.1), lint set, vite-config-driven SvelteKit, Tailwind v4 +
shadcn-svelte ("sera" style), npm workspaces, and the Rust rules (typed errors in libs,
`anyhow` only in binaries, every I/O await bounded, nothing external unwrapped).

## D-001 — The old implementation's baseline is unavailable

The rebuild guide's step 1 ("preserve the old repository; port the black-box failure fixtures")
cannot be done: the previous code is not on this machine or in the workspace archive, and the
maintainer confirms it is gone. Consequences:

- The contracts in `02_Service_Monitor_Rebuild.docx` and the build-log entries on
  404human.dev/service-monitor are the effective specification.
- The failure fixtures must be rebuilt from scratch (`fixtures/`, plus a Node fault server as
  the guide describes). Until they exist, "the rewrite preserves known behavior" cannot be
  claimed — say "rebuilt contract" instead.
- If the old repository resurfaces, archive it under `references/` unchanged and port its
  fixtures before touching internals.
