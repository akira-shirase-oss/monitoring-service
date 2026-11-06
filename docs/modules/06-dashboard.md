# Module 06 — Dashboard (`apps/dashboard`)

**Status:** live (M2–M3, M6.4): list with real status + monitor detail page — recent checks
incl. `missed`, incident timeline (first-fail vs opened vs resolved), coverage-vs-success
line from `GET …/stats` (nulls render as "no data"), unreachable vs missing vs ok states.
Mutations (M6.4): create via the shared `MonitorForm`, detail-page pause/resume, edit
(interval/timeout/thresholds, sent with the `generation` guard), delete behind a two-click
confirm — every write goes through the same API contract, and the server's envelope error
is shown verbatim (stale-generation `conflict`, delete-with-history refusal, 401 → token
hint).

## Purpose

The management and investigation surface: monitors, their current status from committed
evidence, check history and incident timelines — a client of the API, never of the store.

## Contract

- **Static SPA, same origin:** built by `vite build`, served by monitor-server; `ssr = false`;
  no queries to SQLite, no authority.
- **Truthfulness first:** unreachable server → "cannot see" banner (never a green board);
  `unknown` is a displayed state; stale data is labeled with its timestamps.
- **Coverage ≠ success:** check history separates "did we look" (coverage) from "what did we
  see" (outcome). A grey gap in the strip means no observation existed.
- **States vocabulary:** `up`, `down`, `unknown`, `paused` — badges use the status tokens
  (live/attention/draft); pending work uses `pending`.
- **Design:** portfolio tokens (paper/ink, OKLCH), shadcn-svelte components in-app (D-004),
  `#lib` subpath imports, nothing under 14 px, reduced-motion respected.

## Failure cases

| Trigger | Required result |
|---|---|
| API down | "cannot see" banner; no fabricated data |
| Slow API | loading state, never stale-shown-as-fresh |
| Long history | bounded pages; virtualization when justified |

## Testing parameters

- **Unit/component:** badge variant mapping; reachable/unreachable branches.
- **Playwright (M3.4, M6.4):** `tests/dashboard.spec.ts` — API-abort banner (no fabricated
  rows), monitor → `up` badge on committed evidence, real 3-failure incident on the detail
  page (checks strip, "still open", coverage-vs-success line), full mutation spec
  (UI create → pause (API-verified) → edit (generation bumps once per write) → delete),
  delete-with-history refusal surfacing the server message, mutation-401 token hint,
  keyboard navigation. `scripts/e2e.sh` builds the server + dashboard and runs it.
- **Load:** not a target — a local operator tool; page weight budget recorded per release
  (module 09).
