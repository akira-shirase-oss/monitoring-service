#!/usr/bin/env bash
# Stub-based test for schema-guard.sh — no real server, no real database. A stub
# `monitor-server` answers --db-version from $STUB_KNOWN; a stub `sqlite3` answers the
# applied set from $STUB_APPLIED (or fails, when $STUB_SQLITE_FAILS is set). Run by
# tests/deploy_scripts.rs; can also be run by hand: bash deploy/droplet/test-schema-guard.sh
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
t=$(mktemp -d); trap 'rm -rf "$t"' EXIT
mkdir -p "$t/bin"
cat >"$t/bin/monitor-server" <<'EOF'
#!/usr/bin/env bash
if [[ "${1:-}" == --db-version ]]; then
  [[ -n "${STUB_BIN_FAILS:-}" ]] && exit 1
  echo "${STUB_KNOWN:-9}"
else
  exit 2
fi
EOF
cat >"$t/bin/sqlite3" <<'EOF'
#!/usr/bin/env bash
[[ -n "${STUB_SQLITE_FAILS:-}" ]] && { echo "Error: no such table" >&2; exit 1; }
echo "${STUB_APPLIED:-9}"
EOF
chmod +x "$t/bin/monitor-server" "$t/bin/sqlite3"
touch "$t/monitor.db"
export PATH="$t/bin:$PATH"
guard() { STUB_KNOWN=${STUB_KNOWN:-9} STUB_APPLIED=${STUB_APPLIED:-9} \
  STUB_BIN_FAILS=${STUB_BIN_FAILS:-} STUB_SQLITE_FAILS=${STUB_SQLITE_FAILS:-} \
  bash "$here/schema-guard.sh" "$t/monitor.db" "$t/bin/monitor-server"; }
pass=0; fail=0
expect() { # expect <want-exit> <desc> ; reads guard output from $out/$code
  if [[ "$code" -eq "$1" ]]; then pass=$((pass+1)); echo "  ok    $2"; else fail=$((fail+1)); echo "  FAIL  $2 (exit $code: $out)"; fi
}
run() { out=$(guard 2>&1) && code=0 || code=$?; }

run; expect 0 "same version switches"
[[ "$out" == *"9"* ]] || { echo "  FAIL  output should name the version"; fail=$((fail+1)); }

STUB_APPLIED=10; run; expect 1 "database newer than the binary refuses"
[[ "$out" == *"10"* && "$out" == *"9"* ]] && echo "  ok    refusal names both versions" || { echo "  FAIL  refusal must name both versions: $out"; fail=$((fail+1)); }

unset STUB_APPLIED; STUB_KNOWN=12; STUB_APPLIED=9; run; expect 0 "binary newer than the database switches"
unset STUB_KNOWN STUB_APPLIED

STUB_SQLITE_FAILS=1; run; expect 1 "unreadable database fails closed"; unset STUB_SQLITE_FAILS
STUB_APPLIED=garbage; run; expect 1 "non-numeric applied set fails closed"; unset STUB_APPLIED
STUB_BIN_FAILS=1; run; expect 1 "binary that won't report fails closed"; unset STUB_BIN_FAILS

run_guard_missing_db() { "$here/schema-guard.sh" "$t/nope.db" "$t/bin/monitor-server" 2>&1 || true; }
[[ "$(run_guard_missing_db)" == refused:* ]] && echo "  ok    missing database fails closed" || { echo "  FAIL  missing db should refuse"; fail=$((fail+1)); }

echo; echo "test-schema-guard: $pass ok, $fail failed"
[[ $fail -eq 0 ]]
