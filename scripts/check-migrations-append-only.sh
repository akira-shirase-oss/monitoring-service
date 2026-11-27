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
    # N-3 re-review: "no base" is only acceptable locally — in CI a checkout
    # that can't name its main ref must not wave the gate through.
    if [[ -n "${CI:-}" ]]; then
        echo "check-migrations-append-only: no main ref resolvable in CI — refusing" >&2
        exit 1
    fi
    echo "check-migrations-append-only: no main ref to compare against — skipping" >&2
    exit 0
fi
# E re-review: on a push to main the resolved base IS the checked-out commit —
# comparing the manifest to itself always passes. Compare against the parent
# (what main held before this push) instead.
if [[ "$(git rev-parse "$base^{commit}")" == "$(git rev-parse HEAD)" ]]; then
    if git rev-parse --verify --quiet "$base~1^{commit}" >/dev/null; then
        base="$base~1"
    else
        exit 0 # root commit — there is no earlier manifest to keep immutable
    fi
fi
if ! git cat-file -e "$base:$manifest" 2>/dev/null; then
    exit 0 # the base predates the manifest — nothing yet to keep immutable
fi

# N-3 re-review: the base manifest must be a PREFIX of this one — the first
# len(base) non-blank lines identical, in order. "Every base line present
# somewhere" passes an edit that keeps the old line verbatim and shadows it
# with an appended duplicate; prefix stops edits and reorders, and the
# test-side duplicate-name/version checks stop the shadow line itself.
# (Portable: macOS bash 3.2 has no `mapfile`, and process substitution is
# not reliable under restricted environments — use real temp files.)
strip() { sed '/^[[:space:]]*$/d'; }
base_lines=$(mktemp "${TMPDIR:-.}/smon-mig-base.XXXXXX") || exit 1
head_lines=$(mktemp "${TMPDIR:-.}/smon-mig-head.XXXXXX") || { rm -f "$base_lines"; exit 1; }
trap 'rm -f "$base_lines" "$head_lines"' EXIT
git show "$base:$manifest" | strip >"$base_lines"
strip <"$manifest" | head -n "$(wc -l <"$base_lines" | tr -d '[:space:]')" >"$head_lines"
if (( $(wc -l <"$base_lines" | tr -d '[:space:]') > $(strip <"$manifest" | wc -l | tr -d '[:space:]') )); then
    echo "check-migrations-append-only: head manifest is shorter than $base's — a recorded line was removed" >&2
    exit 1
fi
if ! cmp -s "$base_lines" "$head_lines"; then
    echo "check-migrations-append-only: manifest lines differ from $base — applied migrations are immutable:" >&2
    diff -u "$base_lines" "$head_lines" | head -20 >&2
    exit 1
fi

# A pure append can still re-bless: the old line stays verbatim and a shadow
# line gives the same migration file new bytes. Names (and version prefixes —
# one applied version is one file) must be unique across the whole manifest.
dups=$(strip <"$manifest" | awk '{print $2}' | sort | uniq -d)
# Version prefix = the filename's leading digits — `awk -F_ '{print $1}'` would
# yield "<sum>  NNNN" and a duplicate version could never collide.
vers=$(strip <"$manifest" | awk '{n=$2; sub(/_.*/, "", n); print n}' | sort | uniq -d)
if [[ -n "$dups" ]]; then
    echo "check-migrations-append-only: migration named twice — an appended shadow line may never re-bless a file:" >&2
    printf '%s\n' "$dups" >&2
    exit 1
fi
if [[ -n "$vers" ]]; then
    echo "check-migrations-append-only: two lines share a version prefix — one applied version is one file's bytes:" >&2
    printf '%s\n' "$vers" >&2
    exit 1
fi
