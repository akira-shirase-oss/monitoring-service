#!/usr/bin/env bash
# D-011 freeze drill: a whole-process freeze (SIGSTOP, a VM pause, a cgroup freezer)
# is not a stalled coordinator — on resume the monitor must NOT exit, must record the
# lost slots as `missed` rows, and must keep serving honest statuses.
#
#   monitor + fault fixture → checks land → SIGSTOP ~135 s (past the 2-minute
#   stall-exit limit) → SIGCONT → process alive, readyz answers, missed rows
#   recorded, status recovers on fresh probes.
#
# Run from the project root; requires the workspace built (`cargo build`), `curl`,
# `python3` and `node`. Uses its own ports (SMON_FREEZE_PORT, default 58470;
# SMON_FREEZE_FIXTURE_PORT, default 58471) — never 58400, where an installed monitor
# listens — refuses to start if either is taken, and stops only the processes it started.
set -euo pipefail
cd "$(dirname "$0")/.."
# shellcheck source=lib-ports.sh
. scripts/lib-ports.sh

PORT="${SMON_FREEZE_PORT:-58470}"
FIXTURE_PORT="${SMON_FREEZE_FIXTURE_PORT:-58471}"
FREEZE_S="${SMON_FREEZE_S:-135}"   # past the 120 s stall-exit limit (D-011)
require_free_port "$PORT" SMON_FREEZE_PORT
require_free_port "$FIXTURE_PORT" SMON_FREEZE_FIXTURE_PORT
BASE="http://127.0.0.1:$PORT"

DB="$(mktemp -u -t smon-freeze).db"
STATUS_DIR=$(mktemp -d /tmp/smon-freeze-status-XXXX)
LOG=/tmp/smon-freeze-server.log
export SMON_PROFILE=lab SMON_DATABASE_URL="sqlite:$DB" SMON_STATUS_DIR="$STATUS_DIR" \
    SMON_LISTEN="127.0.0.1:$PORT"

SERVER_PID=""
FIXTURE_PID=""
cleanup() {
    stop_own "$SERVER_PID"
    stop_own "$FIXTURE_PID"
    rm -f "$DB"*; rm -rf "$STATUS_DIR"
}
trap cleanup EXIT

node fixtures/fault-server.mjs "$FIXTURE_PORT" >/dev/null 2>&1 &
FIXTURE_PID=$!
wait_own_http "http://127.0.0.1:$FIXTURE_PORT/ok" "$FIXTURE_PID"
./target/debug/monitor-server --init >"$LOG" 2>&1   # D-015: a new store
./target/debug/monitor-server >>"$LOG" 2>&1 &
SERVER_PID=$!
wait_own_http "$BASE/livez" "$SERVER_PID"

# interval_s must respect the domain minimum (MIN_INTERVAL, 15 s): below it the API
# answers a validation error and there is no monitor to drill.
INTERVAL=15
MONITOR=$(curl -s -X POST "$BASE/api/monitors" \
    -H 'content-type: application/json' \
    -d "{\"name\":\"freeze-drill\",\"url\":\"http://127.0.0.1:$FIXTURE_PORT/ok\",\"interval_s\":$INTERVAL,\"timeout_s\":2}" \
    | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')
echo "monitor: $MONITOR — waiting for the first fresh probes…"

# The first slot is scheduled inside the interval window, not at creation (D-018 phase
# spread) — wait 3 × interval + slack, then report what the API saw before failing.
WAIT_FIRST=$((3 * INTERVAL + 10))
for i in $(seq 1 "$WAIT_FIRST"); do
    sleep 1
    STATE=$(curl -s "$BASE/api/monitors/$MONITOR" | python3 -c 'import json,sys; print(json.load(sys.stdin)["status"])' 2>/dev/null || echo "?")
    [ "$STATE" = "up" ] && break
    [ "$i" = "$WAIT_FIRST" ] && {
        echo "FAIL: monitor never came up within ${WAIT_FIRST}s" >&2
        echo "monitor view: $(curl -s "$BASE/api/monitors/$MONITOR")" >&2
        echo "recent checks: $(curl -s "$BASE/api/monitors/$MONITOR/checks?limit=10")" >&2
        echo "last server log lines:" >&2; tail -n 10 "$LOG" >&2
        exit 1
    }
done
echo "up; freezing the whole process for ${FREEZE_S}s (SIGSTOP)…"

kill -STOP "$SERVER_PID"
sleep "$FREEZE_S"
kill -CONT "$SERVER_PID"

# The point of the drill: a freeze is not a stall — the process must still be here.
sleep 2
if ! kill -0 "$SERVER_PID" 2>/dev/null; then
    echo "FAIL: the frozen process exited — a freeze was judged a stall (D-011)"
    exit 1
fi

# On resume: the lost slots land as `missed` rows (the heartbeat stays honest), then
# fresh probes bring the status back to up — never interpolated, never a false recovery.
MISSED=""
STATE=""
# After the resume the missed rows commit at once, then the status needs one fresh slot —
# again inside an interval window, so the same derived wait.
WAIT_RESUME=$((3 * INTERVAL + 10))
for i in $(seq 1 "$WAIT_RESUME"); do
    sleep 1
    MISSED=$(curl -s "$BASE/api/monitors/$MONITOR/checks?limit=100" \
        | python3 -c 'import json,sys; print(sum(1 for c in json.load(sys.stdin)["checks"] if c["outcome"]=="missed"))' 2>/dev/null || echo 0)
    STATE=$(curl -s "$BASE/api/monitors/$MONITOR" | python3 -c 'import json,sys; print(json.load(sys.stdin)["status"])' 2>/dev/null || echo "?")
    [ -n "$MISSED" ] && [ "$MISSED" -ge 5 ] && [ "$STATE" = "up" ] && break
done

READY=$(curl -s -o /dev/null -w '%{http_code}' "$BASE/readyz")
echo "after resume: readyz=$READY missed_rows=$MISSED status=$STATE"

[ "$READY" = "200" ] || { echo "FAIL: readyz answered $READY after the freeze"; exit 1; }
# ~9 slots fall inside a 135 s freeze at the 15 s minimum interval; 5 leaves room for
# slot-phase jitter without letting an unrecorded gap pass.
[ "${MISSED:-0}" -ge 5 ] || { echo "FAIL: only $MISSED missed rows — the gap was not recorded"; exit 1; }
[ "$STATE" = "up" ] || { echo "FAIL: status reads $STATE after resume, not recovered up"; exit 1; }
echo "DRILL PASSED: ${FREEZE_S}s freeze → no exit, $MISSED missed slots recorded, serving resumes honest"
