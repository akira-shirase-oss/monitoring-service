#!/usr/bin/env bash
# Runs what CI will run: the toolchain check, Rust gates, the architecture rule, then the
# dashboard's own checks and build. Run it before every push.
set -euo pipefail
cd "$(dirname "$0")/.."

scripts/check-toolchain.sh

# Nothing that is not a build or runtime artifact may be git-ignored.
if git rev-parse --git-dir >/dev/null 2>&1; then
    scripts/check-ignored.sh
fi
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
# Supply chain: RustSec advisories (cargo-deny; required when CI is set) and deny.toml's
# dated exceptions.
scripts/check-advisories.sh
scripts/check-architecture.sh
npm run check --workspace @service-monitor/dashboard
npm run build --workspace @service-monitor/dashboard
