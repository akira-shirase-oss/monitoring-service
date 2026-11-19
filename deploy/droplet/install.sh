#!/usr/bin/env bash
# The whole install, one command, safe to re-run: every step checks before it does, so a
# second run is a no-op (or repairs a half-finished first one). Run as root on the SFO3
# droplet, from a checkout of this repository:
#
#   sudo deploy/droplet/install.sh --release-dir <dir>   # deploy.sh layout: bin/{monitor-server,monitor}[, dashboard/]
#   sudo deploy/droplet/install.sh                     # binaries already installed — finish the rest
#
# Prompts only for secrets that can't be generated (the origin certificate pair, the R2
# secret), with umask 077 and no echo. Everything else is generated or fetched. ~15 min.
set -euo pipefail
umask 077
PREFIX=/opt/smon
RELEASE_DIR=""
while [ $# -gt 0 ]; do
  case "$1" in
    --release-dir) RELEASE_DIR="$2"; shift 2 ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done
[[ $EUID -eq 0 ]] || { echo "error: run as root" >&2; exit 2; }
here=$(cd "$(dirname "$0")" && pwd)

step() { printf '\n== %s\n' "$1"; }
need() { command -v "$1" >/dev/null 2>&1; }

step "packages"
if ! { need jq && need curl && need age && need rclone && need sqlite3; }; then
  apt-get update
  apt-get install -y ufw jq curl ca-certificates unattended-upgrades age rclone sqlite3
fi
if ! need caddy; then
  apt-get install -y caddy || {
    curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/gpg.key' | gpg --dearmor -o /usr/share/keyrings/caddy-stable-archive-keyring.gpg
    curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/debian.deb.txt' >/etc/apt/sources.list.d/caddy-stable.list
    apt-get update
    apt-get install -y caddy
  }
fi

step "user and directories"
id -u smon >/dev/null 2>&1 || useradd --system --home-dir "$PREFIX" --no-create-home --shell /usr/sbin/nologin smon
install -d -o smon -g smon -m 0700 "$PREFIX/data" "$PREFIX/data/backups" "$PREFIX/status" "$PREFIX/releases"
install -d -m 0700 "$PREFIX/credentials"

step "environment"
if [[ ! -f "$PREFIX/env" ]]; then
  install -m 0600 "$here/smon.env.template" "$PREFIX/env"
fi
if ! grep -q '^SMON_ADMIN_TOKEN=.\+' "$PREFIX/env"; then
  printf 'SMON_ADMIN_TOKEN=%s\n' "$(openssl rand -hex 32)" >>"$PREFIX/env"
  echo "generated the admin token into $PREFIX/env (0600, root only)"
fi
chmod 0600 "$PREFIX/env" && chown root:root "$PREFIX/env"

step "binaries"
if [[ -n "$RELEASE_DIR" ]]; then
  [[ -x "$RELEASE_DIR/bin/monitor-server" && -x "$RELEASE_DIR/bin/monitor" ]] \
    || { echo "error: $RELEASE_DIR doesn't look like a release dir (bin/monitor-server, bin/monitor)" >&2; exit 2; }
  name=$(basename "$RELEASE_DIR")
  mkdir -p "$PREFIX/releases/$name"
  cp -R "$RELEASE_DIR/bin" "$PREFIX/releases/$name/"
  [[ -d "$RELEASE_DIR/dashboard" ]] && cp -R "$RELEASE_DIR/dashboard" "$PREFIX/releases/$name/"
  ln -sfn "$PREFIX/releases/$name" "$PREFIX/current.new" && mv -T "$PREFIX/current.new" "$PREFIX/current"
elif [[ ! -e "$PREFIX/current/bin/monitor-server" ]]; then
  echo "error: no binaries installed and no --release-dir given" >&2; exit 2
fi
"$PREFIX/current/bin/monitor-server" --version

step "the store (created only once — D-015)"
set -a; . "$PREFIX/env"; set +a
dbpath=${SMON_DATABASE_URL#sqlite:}
if sudo -u smon test -f "$dbpath"; then
  echo "store exists at $dbpath — not re-initialising"
else
  sudo -u smon SMON_DATABASE_URL="$SMON_DATABASE_URL" "$PREFIX/current/bin/monitor-server" --init
fi

step "systemd"
install -m 0644 "$here/../monitor-server.service" /etc/systemd/system/monitor-server.service
install -m 0755 "$here/smon-backup.sh" /usr/local/bin/smon-backup
install -m 0755 "$here/schema-guard.sh" /usr/local/bin/smon-schema-guard
install -m 0644 "$here/smon-backup.service" "$here/smon-backup.timer" /etc/systemd/system/
systemctl daemon-reload
systemctl enable --now monitor-server smon-backup.timer

step "Caddy: origin certificate, AOP CA, Cloudflare ranges"
if [[ ! -s /etc/caddy/origin.pem || ! -s /etc/caddy/origin.key ]]; then
  echo "Paste the Cloudflare ORIGIN CERTIFICATE (CREATE.md 7), then Ctrl-D:"
  cat >/etc/caddy/origin.pem
  echo "Paste the ORIGIN PRIVATE KEY, then Ctrl-D:"
  cat >/etc/caddy/origin.key
  chown root:caddy /etc/caddy/origin.pem /etc/caddy/origin.key
  chmod 0640 /etc/caddy/origin.pem /etc/caddy/origin.key
fi
if [[ ! -s /etc/caddy/cloudflare-origin-pull-ca.pem ]]; then
  curl -fsSo /etc/caddy/cloudflare-origin-pull-ca.pem \
    https://developers.cloudflare.com/ssl/static/authenticated_origin_pull_ca.pem
fi
install -m 0644 "$here/Caddyfile" /etc/caddy/Caddyfile
"$here/cloudflare-ips.sh"
caddy validate --config /etc/caddy/Caddyfile
systemctl reload caddy 2>/dev/null || systemctl restart caddy

step "off-box backup: age identity + R2 secret"
if [[ ! -s "$PREFIX/backup-age-identity" ]]; then
  age-keygen -o "$PREFIX/backup-age-identity" && chmod 0400 "$PREFIX/backup-age-identity"
  age-keygen -y "$PREFIX/backup-age-identity" >"$PREFIX/backup-age-recipient"
  echo ">> copy the AGE-SECRET-KEY-… line out of $PREFIX/backup-age-identity NOW (it unlocks every R2 backup), then continue"
fi
if [[ ! -s "$PREFIX/credentials/s3_secret_key" ]]; then
  printf 'Paste the R2 secret access key (no echo): '
  read -rs key || { echo >&2 "aborted: re-run install.sh when you have the key"; exit 2; }
  printf '\n'
  printf '%s' "$key" >"$PREFIX/credentials/s3_secret_key"; unset key
  chmod 0400 "$PREFIX/credentials/s3_secret_key"
  printf 'R2 endpoint (https://<account-id>.r2.cloudflarestorage.com): '
  read -r endpoint
  printf 'R2 access key id: '
  read -r access_key
  printf 'SMON_R2_ENDPOINT=%s\nSMON_R2_ACCESS_KEY=%s\n' "$endpoint" "$access_key" >"$PREFIX/credentials/r2.env"
  chmod 0644 "$PREFIX/credentials/r2.env"; unset endpoint access_key
fi

step "smoke"
"$here/smoke.sh"
echo
echo "install done. Next: DEPLOY.md step 7's CF-only 443 check from your own machine,"
echo "step 8 (the first monitors), step 9 (the restore drill), step 10 (the outside checker)."
