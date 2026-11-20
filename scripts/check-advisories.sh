#!/usr/bin/env bash
# Supply-chain gate (run by check.sh):
#  1. every exception in deny.toml carries a `re-check YYYY-MM-DD` date in its reason, and
#     none of those dates has passed — an accepted advisory is a dated decision, not a
#     permanent silence;
#  2. `cargo deny check advisories bans licenses sources` passes against Cargo.lock: no
#     RustSec advisory, no wildcard requirement, only allow-listed licences, crates.io only
#     (deny.toml). Locally a missing cargo-deny is a warning; in CI (`CI` set) it fails.
# SMON_GATE_TODAY and CARGO_DENY_CMD exist so tests can exercise both rules.
set -euo pipefail
cd "$(dirname "$0")/.."

today="${SMON_GATE_TODAY:-$(date -u +%Y-%m-%d)}"
deny="${CARGO_DENY_CMD:-cargo deny}"

status=0
while IFS= read -r entry; do
    id="$(printf '%s' "$entry" | grep -oE 'RUSTSEC-[0-9]{4}-[0-9]{4}' | head -1 || true)"
    due="$(printf '%s' "$entry" | grep -oE 're-check [0-9]{4}-[0-9]{2}-[0-9]{2}' | head -1 | cut -d' ' -f2 || true)"
    if [ -z "$due" ]; then
        echo "deny.toml: exception ${id:-?} has no 're-check YYYY-MM-DD' in its reason" >&2
        status=1
    elif [[ "$today" > "$due" ]]; then
        echo "deny.toml: exception $id was due for re-check on $due (today is $today):" \
            "re-check the advisory, then renew the date or remove the entry" >&2
        status=1
    fi
done < <(grep -E '^[[:space:]]*\{[[:space:]]*id[[:space:]]*=' deny.toml || true)
[ "$status" = 0 ] || exit 1

# shellcheck disable=SC2086 # "cargo deny" is two words on purpose
if $deny --version >/dev/null 2>&1; then
    $deny --locked check advisories bans licenses sources
    # The fuzz package is a detached workspace — its own lockfile, its own check
    # (model: labs/minibtc's fuzz advisories step).
    if [ -f fuzz/Cargo.lock ]; then
        $deny --manifest-path fuzz/Cargo.toml --locked --config deny.toml \
            check advisories sources
    fi
elif [ -n "${CI:-}" ]; then
    echo "cargo-deny is required in CI: cargo install cargo-deny --locked" >&2
    exit 1
else
    echo "cargo-deny not installed; skipping the advisory check (cargo install cargo-deny)" >&2
fi
