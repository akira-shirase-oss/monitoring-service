# Module 01 — Domain (`crates/monitor-domain`)

**Status:** built (M0). Pure contract, no I/O — `scripts/check-architecture.sh` enforces it.

## Purpose

The engineering contract as types and a pure state machine, so every rule can be tested
deterministically without a database, a network or a clock.

## Contract

- `NewMonitor` validation: non-empty name; `http`/`https` URLs only; `interval >= 15 s`;
  `timeout <= interval` (defaults 60 s / 5 s).
- `CheckOutcome`: `Pass | HttpFail{status} | Transport | Timeout` — an HTTP failure, a timeout
  and a missed observation are different things and are never collapsed.
- `SlotObservation::Missed` is not an outcome — the slot produced no evidence.
- `Health`: 3 consecutive failures open an incident; 2 *fresh* passes confirm recovery; a
  missed slot resets both streaks and returns the display to `unknown` (unless an incident is
  open); an open incident persists across restarts until evidence resolves it (D-005,
  provisional: open + missed currently displays `down` — revisit when the status page lands).
- `Monitor.generation`: bumped on edit/pause; stale-generation results are invalid.

## Failure cases it must express

| Case | Encoding |
|---|---|
| Timeout vs refused vs bad status | distinct `CheckOutcome` variants |
| Coverage gap | `SlotObservation::Missed`, resets streaks |
| Crash before commit | nothing recorded — state machine only sees committed observations |

## Testing parameters

- **Unit:** every transition pair (done); property test candidate: random observation sequences
  must never open an incident on <3 consecutive failures nor recover on <2 fresh passes
  (add with `proptest` when the coordinator lands, to reuse real inputs).
- **Invariant tests:** incident survives rebuild via `Health::with_open_incident` (done).
- **No I/O test:** architecture gate in `scripts/check.sh`.
