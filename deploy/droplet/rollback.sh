#!/usr/bin/env bash
# Roll back to the release before `current` (or to --to <release>): the schema guard runs
# FIRST — migrations are forward-only, so a database newer than the target binary's newest
# known migration refuses before anything switches (naming both versions; restore the
# store's own backup for that case). Then an atomic repoint, restart, readiness poll.
#
#   rollback.sh            # the release installed before current
#   rollback.sh --to vX    # a specific release dir under /opt/smon/releases
set -euo pipefail
PREFIX=/opt/smon
TO=""
while [ $# -gt 0 ]; do
  case "$1" in
    --to) TO="$2"; shift 2 ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done
[[ $EUID -eq 0 ]] || { echo "error: run as root" >&2; exit 2; }
guard=/usr/local/bin/smon-schema-guard
[[ -x "$guard" ]] || guard="$(dirname "$0")/schema-guard.sh"

current=$(readlink -f "$PREFIX/current" || true)
if [[ -n "$TO" ]]; then
  target="$PREFIX/releases/$TO"
  [[ -d "$target" ]] || { echo "error: no release $TO under $PREFIX/releases" >&2; exit 1; }
else
  target=$(ls -1dt "$PREFIX"/releases/*/ | grep -v '/\.staging-' \
    | awk -v c="${current%/}/" '$0 != c' | head -1)
  [[ -n "$target" ]] || { echo "error: no earlier release to roll back to" >&2; exit 1; }
fi
target=${target%/}
[[ "$target" == "$current" ]] && { echo "error: $target IS current" >&2; exit 1; }

dbpath=$(sed -n 's/^SMON_DATABASE_URL=sqlite://p' "$PREFIX/env" | tail -n 1)
dbpath=${dbpath:-$PREFIX/data/monitor.db}
"$guard" "$dbpath" "$target/bin/monitor-server"

ln -sfn "$target" "$PREFIX/current.new" && mv -T "$PREFIX/current.new" "$PREFIX/current"
systemctl restart monitor-server
for _ in $(seq 1 40); do
  curl -fsS --max-time 2 http://127.0.0.1:58400/livez >/dev/null 2>&1 && break
  sleep 0.5
done
curl -fsS --max-time 5 http://127.0.0.1:58400/readyz \
  && echo && echo "rollback: now on $(basename "$target") (was $(basename "$current"))"
