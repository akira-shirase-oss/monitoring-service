#!/usr/bin/env bash
# The migration manifest is append-only (F re-review): a checksum line that
# exists on main must be identical here. Editing an applied migration and
# regenerating the manifest in the same commit must not pass — only appended
# lines for new migrations are allowed. The file-level gate (working tree
# files hash to these sums) is the migrations.rs test; this script gates the
# file-level gate itself against being regenerated.
set -uo pipefail
cd "$(dirname "$0")/.."

manifest=crates/monitor-db/migrations.sha256
[[ -f "$manifest" ]] || { echo "no $manifest to check" >&2; exit 1; }

# The base to compare against: the override, else origin/main or main, else a
# shallow fetch (CI's default checkout is depth 1 and carries no main ref).
base="${SMON_MIGRATIONS_BASE:-}"
if [[ -z "$base" ]]; then
    for cand in origin/main main; do
        if git rev-parse --verify --quiet "$cand^{commit}" >/dev/null; then
            base=$cand
            break
        fi
    done
fi
if [[ -z "$base" ]] && git fetch --depth=1 origin main >/dev/null 2>&1; then
    base=FETCH_HEAD
fi
if [[ -z "$base" ]] || ! git rev-parse --verify --quiet "$base^{commit}" >/dev/null; then
    echo "check-migrations-append-only: no main ref to compare against — skipping" >&2
    exit 0
fi
if ! git cat-file -e "$base:$manifest" 2>/dev/null; then
    exit 0 # the base predates the manifest — nothing yet to keep immutable
fi

# Every line the base recorded must be present, verbatim. A changed or
# removed line means an existing migration's recorded sum moved — applied
# migrations are immutable, so that line may never change.
missing=$(
    git show "$base:$manifest" | while IFS= read -r line; do
        [[ -z "$line" ]] && continue
        grep -qxF -- "$line" "$manifest" || printf '%s\n' "$line"
    done
)
if [[ -n "$missing" ]]; then
    echo "check-migrations-append-only: manifest lines recorded on $base changed:" >&2
    printf '%s\n' "$missing" >&2
    exit 1
fi
