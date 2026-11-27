#!/usr/bin/env bash
# Browser gate (M3.4): builds what Playwright runs against, then runs the specs.
# Kept separate from check.sh — the incident spec waits on a real 3-failure sequence.
set -euo pipefail
cd "$(dirname "$0")/.."

# A Mac with Homebrew's cargo first on PATH needs rustup's proxies ahead of it (the
# toolchain pin); CI's runner has rustup's already and no brew at all.
if command -v brew >/dev/null 2>&1; then
    export PATH="$(brew --prefix rustup)/bin:$PATH"
fi

cargo build --locked -p monitor-server
npm --prefix apps/dashboard run build
# Run from apps/dashboard — playwright resolves its config (baseURL + webServers) by cwd.
(cd apps/dashboard && npx playwright test "$@")
