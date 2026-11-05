#!/usr/bin/env bash
# Runs what CI will run: the toolchain check, Rust gates, the architecture rule, then the
# dashboard's own checks and build. Run it before every push.
set -euo pipefail
cd "$(dirname "$0")/.."

scripts/check-toolchain.sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
scripts/check-architecture.sh
npm run check --workspace @service-monitor/dashboard
npm run build --workspace @service-monitor/dashboard
