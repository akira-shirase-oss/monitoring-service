#!/usr/bin/env bash
# M4.3 restore drill: prove a backup→restore round trip preserves an open incident.
#
#   monitor + fault fixture → drive an incident open → POST /api/backup →
#   crash the server → replace the database file with the snapshot → restart →
#   the incident is still open and the monitor still reads `down`.
#
# Run from the project root; requires the workspace built (`cargo build`), `sqlite3`,
# `curl`, `python3` and `node`. Uses its own ports (SMON_DRILL_PORT, default 58460;
# SMON_DRILL_FIXTURE_PORT, default 58461) — never 58400, where an installed monitor
# listens — refuses to start if either is taken, and stops only the processes it started.
set -euo pipefail
cd "$(dirname "$0")/.."
# shellcheck source=lib-ports.sh
. scripts/lib-ports.sh

PORT="${SMON_DRILL_PORT:-58460}"
FIXTURE_PORT="${SMON_DRILL_FIXTURE_PORT:-58461}"
require_free_port "$PORT" SMON_DRILL_PORT
require_free_port "$FIXTURE_PORT" SMON_DRILL_FIXTURE_PORT
BASE="http://127.0.0.1:$PORT"

DB="$(mktemp -u -t smon-drill).db"
BACKUP_DIR=$(mktemp -d /tmp/smon-drill-backups-XXXX)
STATUS_DIR=$(mktemp -d /tmp/smon-drill-status-XXXX)
LOG=/tmp/smon-drill-server.log
export SMON_PROFILE=lab SMON_DATABASE_URL="sqlite:$DB" SMON_STATUS_DIR="$STATUS_DIR" \
    SMON_LISTEN="127.0.0.1:$PORT" SMON_BACKUP_DIR="$BACKUP_DIR"

SERVER_PID=""
FIXTURE_PID=""
cleanup() {
    stop_own "$SERVER_PID"
    stop_own "$FIXTURE_PID"
    rm -f "$DB"*; rm -rf "$STATUS_DIR" "$BACKUP_DIR"
}
trap cleanup EXIT

node fixtures/fault-server.mjs "$FIXTURE_PORT" >/dev/null 2>&1 &
FIXTURE_PID=$!
wait_own_http "http://127.0.0.1:$FIXTURE_PORT/ok" "$FIXTURE_PID"
./target/debug/monitor-server --init >"$LOG" 2>&1   # D-015: a new store
./target/debug/monitor-server >>"$LOG" 2>&1 &
SERVER_PID=$!
wait_own_http "$BASE/livez" "$SERVER_PID"

INTERVAL=15
MONITOR=$(curl -s -X POST "$BASE/api/monitors" \
    -H 'content-type: application/json' \
    -d "{\"name\":\"drill\",\"url\":\"http://127.0.0.1:$FIXTURE_PORT/fail\",\"interval_s\":$INTERVAL,\"timeout_s\":5}" \
    | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')
echo "monitor: $MONITOR — driving the incident open (~45s of failures)…"

# The first slot lands inside an interval window (D-018 phase spread), then three
# consecutive failures open the incident: ~4 intervals plus slack, not a fixed guess.
WAIT_OPEN=$((4 * INTERVAL + 20))
for i in $(seq 1 $((WAIT_OPEN / 4))); do
    sleep 4
    STATE=$(curl -s "$BASE/api/monitors/$MONITOR" | python3 -c 'import json,sys; print(json.load(sys.stdin)["status"])' 2>/dev/null || echo "?")
    [ "$STATE" = "down" ] && break
    [ "$i" = $((WAIT_OPEN / 4)) ] && {
        echo "FAIL: incident never opened within ${WAIT_OPEN}s" >&2
        echo "monitor view: $(curl -s "$BASE/api/monitors/$MONITOR")" >&2
        echo "recent checks: $(curl -s "$BASE/api/monitors/$MONITOR/checks?limit=10")" >&2
        echo "last server log lines:" >&2; tail -n 10 "$LOG" >&2
        exit 1
    }
done
echo "incident open; backing up…"

# D-019: an empty object asks; the server names the file inside SMON_BACKUP_DIR.
RESPONSE=$(curl -s -X POST "$BASE/api/backup" -H 'content-type: application/json' -d '{}')
echo "$RESPONSE" | tee /dev/stderr
NAME=$(echo "$RESPONSE" | python3 -c 'import json,sys; print(json.load(sys.stdin)["name"])')
BACKUP="$BACKUP_DIR/$NAME"
[ -f "$BACKUP" ] || { echo "FAIL: $NAME not in SMON_BACKUP_DIR"; exit 1; }
BYTES=$(wc -c <"$BACKUP" | tr -d ' ')
[ "$BYTES" -gt 0 ] || { echo "FAIL: empty backup"; exit 1; }

echo; echo "crashing the server (its own PID, SIGKILL) and destroying the live database…"
is_own_child "$SERVER_PID" && kill -9 "$SERVER_PID"
wait "$SERVER_PID" 2>/dev/null || true
SERVER_PID=""
cp "$BACKUP" "$DB"
rm -f "$DB-wal" "$DB-shm"

./target/debug/monitor-server >>"$LOG" 2>&1 &
SERVER_PID=$!
wait_own_http "$BASE/livez" "$SERVER_PID"

STATE=$(curl -s "$BASE/api/monitors/$MONITOR" | python3 -c 'import json,sys; print(json.load(sys.stdin)["status"])')
OPEN=$(curl -s "$BASE/api/incidents?state=open" | python3 -c 'import json,sys; print(len(json.load(sys.stdin)["incidents"]))')
OUTBOX=$(sqlite3 "$DB" "SELECT COUNT(*) FROM notify_outbox WHERE delivered_at IS NULL;")

echo "after restore: status=$STATE open_incidents=$OPEN pending_outbox=$OUTBOX"
[ "$STATE" = "down" ] && [ "$OPEN" -ge 1 ] && [ "$OUTBOX" -ge 1 ] \
    && echo "DRILL PASSED: open incident, status, and pending outbox all survived the restore" \
    || { echo "DRILL FAILED"; exit 1; }
