#!/usr/bin/env bash
# S1a transport-error check: run lab-profile A/B/A/B (pre-S1a binary, then HEAD), keeping
# each run's store so the failing checks can be read per monitor — the summary JSON only
# counts them. SMON_COMMIT labels the result with the binary's commit, not the tree's.
set -euo pipefail
cd "$(dirname "$0")/.."
export SMON_TMP="$PWD/var/smon-tmp"
mkdir -p "$SMON_TMP/ab"

run() { # <label> <binary>
    local label="$1" bin="$2"
    cp "$bin" target/release/monitor-server
    echo "=== $label $(uptime)"
    SMON_COMMIT="$label" node perf/lab-profile.mjs --bin release --mix ok --monitors 100 --duration-s 90 --origins 9
    latest=$(ls -t "$SMON_TMP"/smon-load-*.db | head -1)
    cp "$latest" "$SMON_TMP/ab/$label.db"
    echo "--- stored checks for $label:"
    sqlite3 -json "$SMON_TMP/ab/$label.db" \
        "SELECT m.name, c.outcome, c.http_status, c.detail, c.latency_ms, c.slot_at
           FROM checks c JOIN monitors m ON m.id = c.monitor_id
          WHERE c.outcome != 'pass' ORDER BY c.slot_at"
}

run "ed0d854-a" "$SMON_TMP/monitor-server-ed0d854"
run "head-a"    "$SMON_TMP/monitor-server-head"
run "ed0d854-b" "$SMON_TMP/monitor-server-ed0d854"
run "head-b"    "$SMON_TMP/monitor-server-head"
