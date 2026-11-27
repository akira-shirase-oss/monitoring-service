#!/usr/bin/env bash
# Harness for perf/k6/public-status.js — boots monitor-server on loopback, seeds the
# public route's three answers, runs k6, and writes
# perf/results/public-status-<timestamp>-<mode>-<commit>.json with the machine, the
# commit and the run's shape recorded beside k6's summary (the load-test standard:
# machine + commit travel with the numbers).
#
#   MODE=full  perf/k6/run-public-status.sh   # the ≥1M gate run (CI dispatches this)
#   MODE=smoke perf/k6/run-public-status.sh   # 100k — proves the harness anywhere
#
# The fixture is seeded in the store, not through the API: enabling a page is just
# `status_title` non-NULL, and the writer's s/<slug>.json beat files are what the route
# serves. Both pages are enabled before boot so the first (immediate) beat writes real
# files; SMON_STATUS_INTERVAL_S=3600 holds the fixture static for the whole run — the
# stale page's file is then deleted once, leaving enabled-but-absent, which is exactly
# the born-expired answer (the writer only rewrites it at the next beat, an hour out).
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)

mode="${MODE:-full}"
case "$mode" in
  smoke) requests=100000 ;;
  full)  requests=1000000 ;;
  *) echo "error: MODE must be smoke or full (got '$mode')" >&2; exit 2 ;;
esac
# SM_REQUESTS overrides the mode's count — for development runs only; the committed
# evidence uses the mode's own number.
requests="${SM_REQUESTS:-$requests}"
vus="${SM_VUS:-32}"
port="${SM_PORT:-58560}"
bin="${SMON_SERVER_BIN:-$root/target/release/monitor-server}"
[[ -x "$bin" ]] || { echo "error: no server binary at $bin (cargo build --release -p monitor-server)" >&2; exit 2; }
k6="${K6:-k6}"

work=$(mktemp -d)
server_pid=""
cleanup() {
  [[ -n "$server_pid" ]] && { kill -TERM "$server_pid" 2>/dev/null; for _ in 1 2 3 4 5; do kill -0 "$server_pid" 2>/dev/null || break; sleep 1; done; kill -KILL "$server_pid" 2>/dev/null || true; }
  rm -rf "$work"
}
trap cleanup EXIT

token=$(openssl rand -hex 32)
# SMON_TRUSTED_PROXY=127.0.0.1 is the deployment's own shape: behind Caddy the server
# believes X-Forwarded-For, so the scenario's distinct visitor addresses model real
# traffic instead of one loopback client draining its throttle bucket in seconds.
export SMON_DATABASE_URL="sqlite:$work/monitor.db" SMON_LISTEN="127.0.0.1:$port" \
  SMON_ADMIN_TOKEN="$token" SMON_STATUS_DIR="$work/status" SMON_STATUS_INTERVAL_S=3600 \
  SMON_STATUS_TTL_S=86400 SMON_TRUSTED_PROXY=127.0.0.1

"$bin" --init >/dev/null

# Two workspaces, each with an enabled page: `load-live` on the seeded default
# workspace (its beat file is written by the real snapshot task), `load-stale` on a
# second workspace whose file is deleted after that first beat — the born-expired page.
ws_stale=$(uuidgen 2>/dev/null | tr 'A-Z' 'a-z' || cat /proc/sys/kernel/random/uuid)
sqlite3 "$work/monitor.db" <<SQL
UPDATE workspaces SET slug='load-live', status_title='Load live'
  WHERE id='00000000-0000-0000-0000-000000000001';
INSERT INTO workspaces (id, slug, name, plan, status_title, created_at)
VALUES ('$ws_stale', 'load-stale', 'Load stale', 'operator', 'Load stale',
        strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
SQL

"$bin" >"$work/server.log" 2>&1 & server_pid=$!
for _ in $(seq 1 80); do curl -sf -o /dev/null "http://127.0.0.1:$port/livez" && break; sleep 0.25; done
curl -sf -o /dev/null "http://127.0.0.1:$port/livez" || { echo "error: server never answered /livez"; cat "$work/server.log"; exit 1; }

# Wait for the boot beat to write both page files, then remove the stale one's.
for _ in $(seq 1 40); do
  [[ -f "$work/status/s/load-live.json" && -f "$work/status/s/load-stale.json" ]] && break
  sleep 0.25
done
[[ -f "$work/status/s/load-live.json" ]] || { echo "error: no live page beat was written"; cat "$work/server.log"; exit 1; }
rm -f "$work/status/s/load-stale.json"

# Fixture check — the three answers before the load starts.
code() { curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$port/s/$1/status.json"; }
[[ "$(code load-live)" = 200 ]] || { echo "error: live page does not answer 200"; exit 1; }
[[ "$(code load-stale)" = 200 ]] || { echo "error: stale page does not answer 200"; exit 1; }
[[ "$(code no-such-page)" = 404 ]] || { echo "error: missing page does not answer 404"; exit 1; }

echo "run: $requests requests across $vus VUs against http://127.0.0.1:$port"
SM_BASE="http://127.0.0.1:$port" SM_SLUG=load-live SM_STALE_SLUG=load-stale \
  SM_REQUESTS="$requests" SM_VUS="$vus" SM_SUMMARY="$work/summary.json" \
  "$k6" run "$here/public-status.js"

commit=$(git -C "$root" rev-parse --short HEAD 2>/dev/null || echo unknown)
ts=$(date -u +%Y-%m-%dT%H-%M-%SZ)
out="$root/perf/results/public-status-${ts}-${mode}-${commit}.json"
machine="$(uname -srm)"
if command -v nproc >/dev/null; then cores=$(nproc); else cores=$(sysctl -n hw.ncpu 2>/dev/null || echo 0); fi
if command -v free >/dev/null; then
  mem="$(free -m 2>/dev/null | awk '/^Mem:/ {print $2}') MiB"
else
  mem_bytes=$(sysctl -n hw.memsize 2>/dev/null || echo 0)
  mem="$(( mem_bytes / 1048576 )) MiB"
fi
jq -n --slurpfile k6 "$work/summary.json" \
  --arg scenario "public-status" --arg mode "$mode" --arg commit "$commit" \
  --arg started "$ts" --arg machine "$machine" --arg mem "$mem" --arg loadavg "$(uptime)" \
  --arg binary "$bin" \
  --argjson cores "$cores" --argjson requests "$requests" --argjson vus "$vus" \
  '{scenario:$scenario, mode:$mode, commit:$commit, started:$started,
    machine:{uname:$machine, cores:$cores, memory:$mem, loadavg:$loadavg},
    run:{requests:$requests, vus:$vus, binary:$binary}, k6:$k6[0]}' >"$out"
echo "wrote $out"
