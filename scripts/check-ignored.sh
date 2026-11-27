#!/usr/bin/env bash
# No source may be git-ignored (run by check.sh). Lists every ignored path in the current
# git work tree and fails on any that is not a known build or runtime artifact. An ignore
# rule that swallows source passes every local gate — the files exist here — and breaks
# every clean checkout: an unanchored `status/` once hid the dashboard's /status route.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

artifacts='^((.*/)?\.DS_Store|(.*/)?node_modules/|target/|apps/[^/]+/(\.svelte-kit|build|dist|test-results|playwright-report)/|(.*/)?[^/]+\.db(-shm|-wal)?|mutants\.out[^/]*/|status/|var/|(.*/)?\.env(\..*)?|(.*/)?[^/]+\.swp|\.idea/|\.vscode/|fuzz/(target|artifacts)/)$'
unexpected="$(git ls-files --others --ignored --exclude-standard --directory | grep -Ev "$artifacts" || true)"
if [ -n "$unexpected" ]; then
    echo "git-ignored paths that are not build or runtime artifacts (fix .gitignore):" >&2
    echo "$unexpected" >&2
    exit 1
fi
