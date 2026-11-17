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
BACKUP="$(mktemp -u -t smon-drill).backup.db"
STATUS_DIR=$(mktemp -d /tmp/smon-drill-status-XXXX)
LOG=/tmp/smon-drill-server.log
export SMON_PROFILE=lab SMON_DATABASE_URL="sqlite:$DB" SMON_STATUS_DIR="$STATUS_DIR" \
    SMON_LISTEN="127.0.0.1:$PORT"

SERVER_PID=""
FIXTURE_PID=""
cleanup() {
    stop_own "$SERVER_PID"
    stop_own "$FIXTURE_PID"
    rm -f "$DB"* "$BACKUP"*; rm -rf "$STATUS_DIR"
}
trap cleanup EXIT

node fixtures/fault-server.mjs "$FIXTURE_PORT" >/dev/null 2>&1 &
FIXTURE_PID=$!
wait_http "http://127.0.0.1:$FIXTURE_PORT/ok"
./target/debug/monitor-server --init >"$LOG" 2>&1   # D-015: a new store
./target/debug/monitor-server >>"$LOG" 2>&1 &
SERVER_PID=$!
wait_http "$BASE/livez"

MONITOR=$(curl -s -X POST "$BASE/api/monitors" \
    -H 'content-type: application/json' \
    -d "{\"name\":\"drill\",\"url\":\"http://127.0.0.1:$FIXTURE_PORT/fail\",\"interval_s\":15,\"timeout_s\":5}" \
    | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')
echo "monitor: $MONITOR — driving the incident open (~45s of failures)…"

for i in $(seq 1 16); do
    sleep 4
    STATE=$(curl -s "$BASE/api/monitors/$MONITOR" | python3 -c 'import json,sys; print(json.load(sys.stdin)["status"])')
    [ "$STATE" = "down" ] && break
    [ "$i" = 16 ] && { echo "FAIL: incident never opened"; exit 1; }
done
echo "incident open; backing up…"

curl -s -X POST "$BASE/api/backup" \
    -H 'content-type: application/json' \
    -d "{\"path\":\"$BACKUP\"}" | tee /dev/stderr
BYTES=$(wc -c <"$BACKUP" | tr -d ' ')
[ "$BYTES" -gt 0 ] || { echo "FAIL: empty backup"; exit 1; }

echo; echo "crashing the server (its own PID, SIGKILL) and destroying the live database…"
kill -9 "$SERVER_PID"
wait "$SERVER_PID" 2>/dev/null || true
SERVER_PID=""
cp "$BACKUP" "$DB"
rm -f "$DB-wal" "$DB-shm"

./target/debug/monitor-server >>"$LOG" 2>&1 &
SERVER_PID=$!
wait_http "$BASE/livez"

STATE=$(curl -s "$BASE/api/monitors/$MONITOR" | python3 -c 'import json,sys; print(json.load(sys.stdin)["status"])')
OPEN=$(curl -s "$BASE/api/incidents?state=open" | python3 -c 'import json,sys; print(len(json.load(sys.stdin)["incidents"]))')
OUTBOX=$(sqlite3 "$DB" "SELECT COUNT(*) FROM notify_outbox WHERE delivered_at IS NULL;")

echo "after restore: status=$STATE open_incidents=$OPEN pending_outbox=$OUTBOX"
[ "$STATE" = "down" ] && [ "$OPEN" -ge 1 ] && [ "$OUTBOX" -ge 1 ] \
    && echo "DRILL PASSED: open incident, status, and pending outbox all survived the restore" \
    || { echo "DRILL FAILED"; exit 1; }
