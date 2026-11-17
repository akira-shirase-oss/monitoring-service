#!/usr/bin/env bash
# Runs the server with the local SQLite file (monitor.db, git-ignored). Serves the dashboard
# build on the same origin when apps/dashboard/build exists; otherwise API only.
# For dashboard development with hot reload: npm run dev --workspace @service-monitor/dashboard
# (vite proxies /api, /livez and /readyz to the server on 58400).
set -euo pipefail
cd "$(dirname "$0")/.."
# D-015: a normal start needs an existing store; `--init` creates (or just migrates) it.
cargo run -q -p monitor-server -- --init
exec cargo run -p monitor-server
