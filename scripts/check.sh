#!/usr/bin/env bash
# Runs what CI will run: the toolchain check, Rust gates, the architecture rule, then the
# dashboard's own checks and build. Run it before every push.
set -euo pipefail
cd "$(dirname "$0")/.."

scripts/check-toolchain.sh

# Nothing under a source directory may be git-ignored. An ignore rule that swallows source
# passes every local gate (the files exist here) and breaks every clean checkout — an
# unanchored `status/` once hid the dashboard's /status route this way.
if git rev-parse --git-dir >/dev/null 2>&1; then
    # Unquoted on purpose: the shell expands the globs to real directories.
    ignored_src="$(git ls-files --others --ignored --exclude-standard -- \
        apps/*/src apps/status crates/*/src crates/*/tests crates/*/migrations \
        scripts deploy docs fixtures)"
    if [ -n "$ignored_src" ]; then
        echo "git-ignored files under source directories (fix .gitignore):" >&2
        echo "$ignored_src" >&2
        exit 1
    fi
fi
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
scripts/check-architecture.sh
npm run check --workspace @service-monitor/dashboard
npm run build --workspace @service-monitor/dashboard
