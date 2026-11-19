#!/usr/bin/env bash
# The forward-only-migrations guard (upgrade.sh and rollback.sh both run it before
# repointing `current`): refuse to point the service at a binary whose newest known
# migration is BEHIND the database's newest applied one — an older binary on a newer
# schema is not a rollback, it is a crash loop that re-runs this same lesson
# (Durable Runner's D-056). Fails CLOSED: an unreadable database, a binary that won't
# report, or a non-numeric answer all refuse. Usage:
#
#   schema-guard.sh <database-file> <monitor-server-binary>
#
# exit 0 = safe to switch; exit 1 = refused (both versions are named); exit 2 = usage.
set -euo pipefail
[[ $# -eq 2 ]] || { echo "usage: schema-guard.sh <database-file> <server-binary>" >&2; exit 2; }
db=$1; bin=$2
[[ -f "$db" ]]   || { echo "refused: no database at $db (nothing to compare — is the volume mounted?)" >&2; exit 1; }
[[ -x "$bin" ]]  || { echo "refused: $bin is not an executable monitor-server" >&2; exit 1; }

if ! known=$("$bin" --db-version 2>/dev/null) || ! [[ "$known" =~ ^[0-9]+$ ]]; then
  echo "refused: $bin did not report a schema version (its '--db-version' gave '${known:-nothing}')" >&2
  exit 1
fi
# sqlx records applied migrations in _sqlx_migrations; a failed run leaves success=0 rows
# that must not count — the newest SUCCESSFUL migration is the schema this database speaks.
if ! applied=$(sqlite3 -readonly "$db" \
    "SELECT COALESCE(MAX(version), 0) FROM _sqlx_migrations WHERE success = 1;" 2>/dev/null) \
    || ! [[ "$applied" =~ ^[0-9]+$ ]]; then
  echo "refused: cannot read the applied migration set from $db" >&2
  exit 1
fi
if (( applied > known )); then
  echo "refused: database $db is at migration $applied; $bin only knows $known — a rollback this far back needs the store restored from its own backup first" >&2
  exit 1
fi
echo "schema-guard: database at migration $applied, binary knows $known — safe"
