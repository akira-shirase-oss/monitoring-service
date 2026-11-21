#!/usr/bin/env bash
# The rehearsed upgrade (deploy/droplet/DEPLOY.md): stage the new release, guard the schema,
# switch `current` atomically, restart, prove readiness, roll back automatically if it isn't
# healthy. Two artifact forms:
#
#   upgrade.sh --release-dir <dir>                     # a built release dir (bin/{server,cli}[, dashboard/])
#   upgrade.sh --version <v> --url <tarball> --sha256 <hex>   # download + verify, then stage
#
# The switch itself is one rename; the serving gap is the restart — under Caddy's
# lb_try_duration requests wait out the gap (< 5 s target), while check slots whose deadline
# fell inside it are committed `missed` on boot (D-014: the monitor accounts for its own
# downtime exactly as it accounts for a target's). Rollback is automatic on any post-switch
# failure: repoint to the previous release, restart, poll, report — and exit non-zero.
# Keeps the newest 3 release dirs.
set -euo pipefail
PREFIX=/opt/smon
RELEASE_DIR=""; VERSION=""; URL=""; SHA=""
while [ $# -gt 0 ]; do
  case "$1" in
    --release-dir) RELEASE_DIR="$2"; shift 2 ;;
    --version) VERSION="$2"; shift 2 ;;
    --url) URL="$2"; shift 2 ;;
    --sha256) SHA="$2"; shift 2 ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done
[[ $EUID -eq 0 ]] || { echo "error: run as root" >&2; exit 2; }
guard=/usr/local/bin/smon-schema-guard
[[ -x "$guard" ]] || guard="$(dirname "$0")/schema-guard.sh"

# --- stage -------------------------------------------------------------
staging=""
if [[ -n "$RELEASE_DIR" ]]; then
  VERSION=${VERSION:-$(basename "$RELEASE_DIR")}
  staging="$PREFIX/releases/.staging-$VERSION"
  rm -rf "$staging"; mkdir -p "$staging"
  cp -R "$RELEASE_DIR/bin" "$staging/"
  [[ -d "$RELEASE_DIR/dashboard" ]] && cp -R "$RELEASE_DIR/dashboard" "$staging/"
elif [[ -n "$VERSION" && -n "$URL" && -n "$SHA" ]]; then
  staging="$PREFIX/releases/.staging-$VERSION"
  rm -rf "$staging"; mkdir -p "$staging"
  pkg="$staging/package"; dl=$(mktemp)
  curl -fsSL --max-time 300 "$URL" -o "$dl"
  echo "$SHA  $dl" | sha256sum -c - >/dev/null
  mkdir -p "$pkg" && tar -xzf "$dl" -C "$pkg" && rm -f "$dl"
  inner=$(find "$pkg" -name monitor-server -type f | head -1)
  [[ -n "$inner" ]] || { echo "error: no monitor-server in the package" >&2; rm -rf "$staging"; exit 1; }
  root=$(dirname "$(dirname "$inner")")        # <root>/bin/monitor-server
  cp -R "$root/bin" "$staging/"
  [[ -d "$root/dashboard" ]] && cp -R "$root/dashboard" "$staging/"
  rm -rf "$pkg"
else
  echo "usage: upgrade.sh --release-dir <dir> | --version <v> --url <tarball> --sha256 <hex>" >&2; exit 2
fi
[[ -x "$staging/bin/monitor-server" ]] || { echo "error: staged release has no monitor-server" >&2; rm -rf "$staging"; exit 1; }

previous=$(readlink -f "$PREFIX/current" || true)
dest="$PREFIX/releases/$VERSION"
rm -rf "$dest" && mv -T "$staging" "$dest"
echo "upgrade: staged $dest (binary: $("$dest/bin/monitor-server" --version))"

# --- schema guard BEFORE the switch -------------------------------------
dbpath=$(sed -n 's/^SMON_DATABASE_URL=sqlite://p' "$PREFIX/env" | tail -n 1)
dbpath=${dbpath:-$PREFIX/data/monitor.db}
"$guard" "$dbpath" "$dest/bin/monitor-server"

# --- applied-migration checksum inspection (read-only, before the switch) --
# A database that ever ran a branch/experiment build may hold _sqlx_migrations
# rows whose bytes differ from this release's migrations — sqlx would refuse to
# boot AFTER the switch. The candidate binary carries its own compiled-in
# manifest (--migration-manifest), so no source tree or migrations/ directory
# has to exist on the host. A divergence, a failed row, or a query that cannot
# answer aborts the upgrade; the check never repairs — that is a human call.
inspector=/usr/local/bin/smon-inspect-migrations
[[ -x "$inspector" ]] || inspector="$(dirname "$0")/inspect-migrations.sh"
"$inspector" "$dbpath" "$dest/bin/monitor-server"

# --- atomic switch + restart + prove -------------------------------------
switch() { ln -sfn "$1" "$PREFIX/current.new" && mv -T "$PREFIX/current.new" "$PREFIX/current"; }
wait_ready() {
  for _ in $(seq 1 40); do  # 20 s ceiling — well under Caddy's 15 s hold + margin
    curl -fsS --max-time 2 http://127.0.0.1:58400/livez >/dev/null 2>&1 && return 0
    sleep 0.5
  done
  return 1
}
switch "$dest"
systemctl restart monitor-server

if wait_ready; then
  if SMOKE_LOCAL=1 "$(dirname "$0")/smoke.sh"; then
    echo "upgrade: $VERSION is live and smokes clean"
  else
    echo "upgrade: $VERSION serves but smoke failed — rolling back" >&2
    switch "$previous"; systemctl restart monitor-server; wait_ready || true
    exit 1
  fi
else
  echo "upgrade: $VERSION never became ready — rolling back to $(basename "$previous")" >&2
  [[ -n "$previous" ]] || { echo "error: no previous release to return to" >&2; exit 1; }
  switch "$previous"; systemctl restart monitor-server
  wait_ready || { echo "rollback did not come ready — investigate now" >&2; exit 1; }
  echo "upgrade: rolled back to $(basename "$previous")"
  exit 1
fi

# --- keep the newest 3 ----------------------------------------------------
ls -1dt "$PREFIX"/releases/*/ | grep -v '/\.staging-' | tail -n +4 | while read -r old; do
  [[ "$old" == "$dest/" || "$old" == "$dest" ]] && continue
  rm -rf "$old" && echo "pruned release $old"
done
echo "upgrade: done — $VERSION, rollback target was $(basename "${previous:-none}")"
