#!/usr/bin/env bash
# Runs the server with the local SQLite file (monitor.db, git-ignored). Serves the dashboard
# build on the same origin when apps/dashboard/build exists; otherwise API only.
# For dashboard development with hot reload: npm run dev --workspace @service-monitor/dashboard
# (vite proxies /api, /livez and /readyz to this server).
#
# Listens on its own port, SMON_DEV_PORT (default 58465) — never 58400, where an installed
# monitor listens — and refuses to start if that port is taken. Point the CLI at it with
# SMON_SERVER=http://127.0.0.1:58465; vite reads SMON_DEV_PORT too.
set -euo pipefail
cd "$(dirname "$0")/.."
# shellcheck source=lib-ports.sh
. scripts/lib-ports.sh

PORT="${SMON_DEV_PORT:-58465}"
require_free_port "$PORT" SMON_DEV_PORT
export SMON_LISTEN="127.0.0.1:$PORT"
# D-015: a normal start needs an existing store; `--init` creates (or just migrates) it.
cargo run -q -p monitor-server -- --init
exec cargo run -p monitor-server
