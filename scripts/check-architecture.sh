#!/usr/bin/env bash
# monitor-domain is the contract in pure form: no async runtimes, HTTP or storage crates may
# appear in its [dependencies]. Keep it that way so every transition stays cheap to test.
set -euo pipefail
cd "$(dirname "$0")/.."

bad="$(sed -n '/^\[dependencies\]/,/^\[/p' crates/monitor-domain/Cargo.toml \
  | grep -nE 'tokio|axum|sqlx|reqwest|tower|hyper' || true)"
if [ -n "$bad" ]; then
  echo "error: monitor-domain must not depend on I/O crates:" >&2
  echo "$bad" >&2
  exit 1
fi
echo "architecture ok: monitor-domain has no I/O dependencies"
