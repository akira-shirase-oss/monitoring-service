#!/usr/bin/env bash
# M4.3 restore drill: prove a backup→restore round trip preserves an open incident.
#
#   monitor + fault fixture → drive an incident open → POST /api/backup →
#   stop → replace the database file with the snapshot → restart →
#   the incident is still open and the monitor still reads `down`.
#
# Run from the project root; requires the workspace built (`cargo build`) and `sqlite3`.
set -euo pipefail
cd "$(dirname "$0")/.."

DB="$(mktemp -u -t smon-drill).db"
BACKUP="$(mktemp -u -t smon-drill).backup.db"
STATUS_DIR=$(mktemp -d /tmp/smon-drill-status-XXXX)
export SMON_PROFILE=lab SMON_DATABASE_URL="sqlite:$DB" SMON_STATUS_DIR="$STATUS_DIR"

cleanup() {
    lsof -ti :58400 -ti :58700 2>/dev/null | xargs kill -9 2>/dev/null || true
    rm -f "$DB"* "$BACKUP"*; rm -rf "$STATUS_DIR"
}
trap cleanup EXIT

node fixtures/fault-server.mjs >/dev/null 2>&1 &
sleep 1
./target/debug/monitor-server >/tmp/smon-drill-server.log 2>&1 &
sleep 2

MONITOR=$(curl -s -X POST http://127.0.0.1:58400/api/monitors \
    -H 'content-type: application/json' \
    -d '{"name":"drill","url":"http://127.0.0.1:58700/fail","interval_s":15,"timeout_s":5}' \
    | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')
echo "monitor: $MONITOR — driving the incident open (~45s of failures)…"

for i in $(seq 1 16); do
    sleep 4
    STATE=$(curl -s "http://127.0.0.1:58400/api/monitors/$MONITOR" | python3 -c 'import json,sys; print(json.load(sys.stdin)["status"])')
    [ "$STATE" = "down" ] && break
    [ "$i" = 16 ] && { echo "FAIL: incident never opened"; exit 1; }
done
echo "incident open; backing up…"

curl -s -X POST http://127.0.0.1:58400/api/backup \
    -H 'content-type: application/json' \
    -d "{\"path\":\"$BACKUP\"}" | tee /dev/stderr
BYTES=$(stat -f%z "$BACKUP")
[ "$BYTES" -gt 0 ] || { echo "FAIL: empty backup"; exit 1; }

echo; echo "stopping the server and destroying the live database…"
lsof -ti :58400 | xargs kill -9 2>/dev/null; sleep 1
cp "$BACKUP" "$DB"
rm -f "$DB-wal" "$DB-shm"

./target/debug/monitor-server >>/tmp/smon-drill-server.log 2>&1 &
sleep 2

STATE=$(curl -s "http://127.0.0.1:58400/api/monitors/$MONITOR" | python3 -c 'import json,sys; print(json.load(sys.stdin)["status"])')
OPEN=$(curl -s "http://127.0.0.1:58400/api/incidents?state=open" | python3 -c 'import json,sys; print(len(json.load(sys.stdin)["incidents"]))')
OUTBOX=$(sqlite3 "$DB" "SELECT COUNT(*) FROM notify_outbox WHERE delivered_at IS NULL;")

echo "after restore: status=$STATE open_incidents=$OPEN pending_outbox=$OUTBOX"
[ "$STATE" = "down" ] && [ "$OPEN" -ge 1 ] && [ "$OUTBOX" -ge 1 ] \
    && echo "DRILL PASSED: open incident, status, and pending outbox all survived the restore" \
    || { echo "DRILL FAILED"; exit 1; }
