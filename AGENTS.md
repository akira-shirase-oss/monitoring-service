# Service Monitor: agent instructions

The rebuild of the old service monitor, per `Project_Guides/02_Service_Monitor_Rebuild.docx`
(read it first — it is the contract). The old code is gone (D-001); the guide plus the
404human.dev build log are the specification.

**Current state (2026-11-05): M1 "check and remember" done.** The observing loop is real:
coordinator (`schedule.rs`, one attempt per slot, `try_acquire` bounds, counted misses),
probe engine (`probe.rs`, GET only, deadline + 64 KiB body bound, classified outcomes,
URL-layer target policy), single-writer task (`monitor-db::writer`, check + incident +
outbox in one transaction), `Health::rebuild` restart recovery, and statuses in the API that
only reflect committed evidence. No notification delivery, monitor edit/pause, incidents
API, auth, DNS-level SSRF guards, or status renderer yet. Next slice: M2 Notify — the outbox
deliverer, monitor management writes, incidents API, fault fixtures. `SMON_PROFILE=lab`
permits loopback targets for fixtures.

## Read first

1. `docs/END_GOAL.md` (what done means) → `docs/ARCHITECTURE.md` (sources of truth, process
   model) → `docs/ROADMAP.md` (milestones; each step carries outcome, invariant, failure case,
   acceptance test) → the module doc for the area you are touching under `docs/modules/`.
2. `docs/DECISIONS.md` — permanent decisions (D-001 baseline missing, D-005 provisional status
   semantics). Record new decisions here.
3. `Project_Guides/02_Service_Monitor_Rebuild.docx` — the engineering contract. When code and
   guide disagree, the guide wins unless a `D-NNN` says otherwise.

## What this project optimizes for

**Honest status over reassuring status.** A dead monitor reports "monitoring unavailable", never
"all targets down". A disk failure shows unavailable, never green. `unknown` is a first-class
state, not an error to hide. Proof over breadth: a feature counts when its failure behavior is
tested.

## Non-negotiable invariants (from the guide)

1. **SQLite is the authority** for monitors, checks, incidents and the notification outbox.
   WAL mode, foreign keys, migrations in `crates/monitor-db/migrations/`.
2. **Observation, incident transition and notification outbox commit together** — one
   transaction, never three separate writes.
3. **Missed checks reset streaks and display `unknown`**; they never count as passes. Open
   incidents persist until two *fresh* passes close them — across restarts.
4. **One attempt per slot** — no overlap, no catch-up storm, bounded response size, bounded
   concurrency (global slots and per-origin).
5. **Probes are safe `GET`/`HEAD` to authorized targets.** SSRF guards (redirects, private IPs,
   DNS rebinding) are mandatory before probing; localhost only in an explicit lab profile.
6. **Generation invalidation**: edit/pause bumps `generation`; a late result from an older
   generation must not update current status.
7. **`monitor-domain` holds no I/O** (`scripts/check-architecture.sh` enforces).

## Rust rules

- Use the `rust-mastery` skill: read `~/Desktop/Learning/rust-mastery/AGENT_PLAYBOOK.md` and the
  chapters it routes to (07 and 12 are mandatory for async and service code).
- The toolchain is pinned to Rust 1.99.1. Homebrew `cargo`/`rustc` 1.96.0 comes first on `PATH`
  and **ignores the pin** — prefix every Cargo command with rustup's proxies:
  `export PATH="$(brew --prefix rustup)/bin:$PATH"`. `scripts/check-toolchain.sh` fails if you forget.
- Typed errors (`thiserror`) in libraries; `anyhow` only in the binaries. No `unwrap`/`expect`
  on external input. Every I/O await has a deadline. Every queue, channel, body and buffer is
  bounded. No secrets or target internals in logs or error envelopes.

## Frontend rules

- SvelteKit + TypeScript (strict), Tailwind CSS v4, shadcn-svelte ("sera"). A static SPA:
  `ssr = false`, built by `vite build` into `build/` and served by monitor-server on the same
  origin. It never queries SQLite or holds authority.
- The dashboard tells the truth about the monitor: unreachable server → "cannot see" banner,
  never a green empty board.

## Testing rule

Every feature ships with regression tests in the same branch: Rust unit tests (domain logic),
in-process API tests (`crates/monitor-server/tests/`), and later black-box fixtures in
`fixtures/` plus a load scenario in `perf/k6/`. Every fixed bug gets a test that fails without
the fix. Report exactly what ran and what passed; never claim verified/benchmarked without an
executed run.

## Verification commands

`scripts/check.sh` — toolchain check, `cargo fmt --all -- --check`,
`cargo clippy --workspace --all-targets --locked -- -D warnings`, `cargo test --workspace --locked`,
`scripts/check-architecture.sh`, `svelte-check` and a dashboard build. Run before every push.

## Working rules

- Work in small slices; each states the user outcome, the invariant, the failure case, the
  permission, and the acceptance test.
- Numbers in docs are proposed targets, not measurements. Never present laptop numbers as
  production capacity.
- Repository will be `github.com/akira-shirase-oss/service-monitor`; work on a branch, merge to
  `main` when a piece is complete; build log on 404human.dev.
