#!/usr/bin/env bash
# Browser gate (M3.4): builds what Playwright runs against, then runs the specs.
# Kept separate from check.sh — the incident spec waits on a real 3-failure sequence.
set -euo pipefail
cd "$(dirname "$0")/.."

export PATH="$(brew --prefix rustup)/bin:$PATH"

cargo build -p monitor-server
npm --prefix apps/dashboard run build
npm --prefix apps/dashboard exec -- playwright test "$@"
