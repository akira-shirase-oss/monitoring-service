#!/usr/bin/env bash
# inspect-migrations.sh <database-path> <monitor-server-binary>
#
# The applied-migration inspection upgrade.sh runs before the atomic switch:
# a database that ever ran a branch/experiment build can hold _sqlx_migrations
# rows whose bytes differ from this release's compiled-in migrations — and
# sqlx would refuse to boot only AFTER the switch. Compare what was applied
# (hex(checksum) — sqlx stores SHA-384, the same digest shasum -a 384 prints)
# against the candidate binary's own --migration-manifest while the previous
# release still serves. Read-only (mode=ro, WAL-safe) and it never repairs —
# divergence aborts the upgrade for a human call, it is not edited away.
#
# Exits 0 when every applied row matches the binary's manifest; exits 1 on any
# divergence, a failed (success=0) row, or a query error — a check that cannot
# answer must not wave the switch through.
set -euo pipefail

db=${1:-}; bin=${2:-}
[[ -n "$db" && -n "$bin" ]] || {
  echo "usage: inspect-migrations.sh <database-path> <monitor-server-binary>" >&2
  exit 2
}
[[ -x "$bin" ]] || { echo "inspect-migrations: $bin is not executable" >&2; exit 1; }
command -v sqlite3 >/dev/null 2>&1 || {
  echo "inspect-migrations: sqlite3 is required" >&2
  exit 1
}
[[ -f "$db" ]] || {
  echo "inspect-migrations: no database at $db — nothing applied to inspect"
  exit 0
}

manifest=$("$bin" --migration-manifest)
# -batch exits nonzero on an SQL error; .timeout bounds a busy database instead
# of waiting out the kernel-level lock; -readonly never writes, not even
# journal files (the file:...?mode=ro URI is not portable across sqlite3 CLIs).
if ! rows=$(sqlite3 -batch -readonly "$db" -cmd ".timeout 5000" \
  "SELECT version, lower(hex(checksum)), success FROM _sqlx_migrations ORDER BY version;"); then
  echo "inspect-migrations: could not read $db — refusing to continue blind" >&2
  exit 1
fi

diverged=0
while IFS='|' read -r v hexsum ok; do
  [[ -z "$v" ]] && continue
  want=$(printf '%s\n' "$manifest" | awk -F'|' -v v="$v" '$1 == v { print $2; exit }')
  file=$(printf '%s\n' "$manifest" | awk -F'|' -v v="$v" '$1 == v { print $3; exit }')
  if [[ -z "$want" ]]; then
    echo "inspect-migrations: applied v$v has no migration in this binary — a branch-build row?" >&2
    diverged=1
    continue
  fi
  if [[ "$ok" != "1" ]]; then
    echo "inspect-migrations: applied v$v ($file) is recorded failed (success=0) — half-applied" >&2
    diverged=1
    continue
  fi
  if [[ "$hexsum" != "$want" ]]; then
    echo "inspect-migrations: applied v$v checksum diverges from this binary's $file" >&2
    diverged=1
  fi
done <<<"$rows"

if (( diverged )); then
  echo "inspect-migrations: divergence — inspect _sqlx_migrations by hand (DEPLOY.md); the check never repairs" >&2
  exit 1
fi
echo "inspect-migrations: applied migrations match this binary's manifest"
