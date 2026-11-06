#!/usr/bin/env bash
# Install monitor-server into a versioned prefix: releases/<release>/bin/monitor-server
# with a `current` symlink the service unit points at. Upgrade = new release dir +
# repoint + restart. Rollback = repoint to the previous release + restart — the store
# lives outside releases/, so data survives both.
#
#   scripts/deploy.sh --prefix /opt/smon [--release NAME] [--skip-build]
#
# The monitor must run on its own host, probing *at* the targets — never on the same
# box it watches (a shared failure would lie about both).
set -euo pipefail
cd "$(dirname "$0")/.."

PREFIX=/opt/smon
RELEASE="$(git rev-parse --short HEAD)"
SKIP_BUILD=0
while [ $# -gt 0 ]; do
    case "$1" in
        --prefix) PREFIX="$2"; shift 2 ;;
        --release) RELEASE="$2"; shift 2 ;;
        --skip-build) SKIP_BUILD=1; shift ;;
        *) echo "unknown arg: $1" >&2; exit 2 ;;
    esac
done

if [ "$SKIP_BUILD" = 0 ]; then
    cargo build --release -p monitor-server -p monitor-cli
fi

DEST="$PREFIX/releases/$RELEASE"
mkdir -p "$DEST/bin" "$PREFIX/data" "$PREFIX/status"
cp target/release/monitor-server target/release/monitor "$DEST/bin/"
ln -sfn "$DEST" "$PREFIX/current"

[ -f "$PREFIX/env" ] || cat > "$PREFIX/env" <<'ENV'
# SMON_ADMIN_TOKEN=   # REQUIRED on any non-loopback SMON_LISTEN — boot refuses without it
# SMON_DATABASE_URL=sqlite:/opt/smon/data/monitor.db
# SMON_LISTEN=127.0.0.1:58400
# SMON_STATUS_DIR=/opt/smon/status
# SMON_DASHBOARD_DIR=/opt/smon/current/dashboard
# SMON_MAX_DB_BYTES=2147483648
# SMON_RETENTION_S=604800
# SMON_LOG_FILE=/opt/smon/data/monitor.log
# SMON_WEBHOOK_URL=
# SMON_WEBHOOK_SECRET=
# Email channel — set all three or none (partial config refuses to boot):
# SMON_SMTP_DSN=smtps://user:pass@smtp.example.com:465   # or smtp://host:587?tls=required
# SMON_SMTP_FROM=alerts@example.com
# SMON_SMTP_TO=oncall@example.com,backup@example.com
ENV

if [ -d apps/dashboard/build ]; then
    rm -rf "$DEST/dashboard"
    cp -R apps/dashboard/build "$DEST/dashboard"
fi

cat <<DONE
installed $RELEASE → $DEST
  current → $PREFIX/current
  env     → $PREFIX/env  (edit SMON_* before starting)
data lives in $PREFIX/data (outside releases — upgrades and rollbacks don't touch it)
next: install deploy/monitor-server.service or deploy/com.shirase.smon.plist
DONE
