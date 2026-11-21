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
#
# Non-interactive installs (CI, replays) pass the same secrets as file paths — never
# through a prompt and never as argv values (ps shows argv to every local user):
# --origin-cert F --origin-key F each replaces its `cat` prompt, and --r2-secret F
# --r2-env F together replace the R2 prompts (the env file carries SMON_R2_ENDPOINT
# and SMON_R2_ACCESS_KEY; a partial set is an error — prompting for half a credential
# is worse than either mode).
set -euo pipefail
umask 077
PREFIX=/opt/smon
RELEASE_DIR=""
ORIGIN_CERT=""; ORIGIN_KEY=""
R2_SECRET=""; R2_ENV=""
while [ $# -gt 0 ]; do
  case "$1" in
    --release-dir)    RELEASE_DIR="$2"; shift 2 ;;
    --origin-cert)    ORIGIN_CERT="$2"; shift 2 ;;
    --origin-key)     ORIGIN_KEY="$2"; shift 2 ;;
    --r2-secret)      R2_SECRET="$2"; shift 2 ;;
    --r2-env)         R2_ENV="$2"; shift 2 ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done
for f in "$ORIGIN_CERT" "$ORIGIN_KEY" "$R2_SECRET" "$R2_ENV"; do
  [[ -z "$f" || -f "$f" ]] || { echo "error: $f does not exist" >&2; exit 2; }
done
if [[ -n "$R2_SECRET" || -n "$R2_ENV" ]] && [[ -z "$R2_SECRET" || -z "$R2_ENV" ]]; then
  echo "error: --r2-secret and --r2-env go together" >&2; exit 2
fi
if [[ -n "$R2_ENV" ]]; then
  for key in SMON_R2_ENDPOINT SMON_R2_ACCESS_KEY; do
    grep -q "^$key=" "$R2_ENV" || { echo "error: $R2_ENV lacks $key" >&2; exit 2; }
  done
fi
[[ $EUID -eq 0 ]] || { echo "error: run as root" >&2; exit 2; }
here=$(cd "$(dirname "$0")" && pwd)

step() { printf '\n== %s\n' "$1"; }
need() { command -v "$1" >/dev/null 2>&1; }

step "packages"
if ! { need jq && need curl && need age && need rclone && need sqlite3; }; then
  apt-get update
  apt-get install -y ufw jq curl ca-certificates unattended-upgrades age rclone sqlite3
fi
# The pinned caddy repo is written UNCONDITIONALLY (N5 review): a box where
# caddy is already installed still needs it — its next `apt-get upgrade`
# otherwise pulls from whatever unverifiable source it already had.
# The repo key is pinned by fingerprint (S2 review): import the fetched key file
# into a throwaway keyring and export ONLY the pinned key — dearmoring the whole
# file would trust every other key bundled in it.
caddy_key=$(mktemp)
curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/gpg.key' -o "$caddy_key"
want=65760C51EDEA2017CEA2CA15155B6D79CA56EA34
gtmp=$(mktemp -d); chmod 0700 "$gtmp"
gpg --batch --homedir "$gtmp" --import "$caddy_key" >/dev/null 2>&1
gpg --batch --homedir "$gtmp" --export "$want" \
  >/usr/share/keyrings/caddy-stable-archive-keyring.gpg \
  || { echo "error: caddy repo key $want not present in fetched key file" >&2;
       rm -rf "$gtmp" "$caddy_key"; exit 2; }
[[ -s /usr/share/keyrings/caddy-stable-archive-keyring.gpg ]] \
  || { echo "error: pinned caddy repo key exported empty" >&2;
       rm -rf "$gtmp" "$caddy_key"; exit 2; }
rm -rf "$gtmp" "$caddy_key"
# The sources line is written inline (S3 review): fetching the .list means
# trusting whatever signed-by/trusted=yes it arrives carrying — that would
# undo the fingerprint pin above.
printf '%s\n' \
  'deb [signed-by=/usr/share/keyrings/caddy-stable-archive-keyring.gpg] https://dl.cloudsmith.io/public/caddy/stable/deb/debian any-version main' \
  >/etc/apt/sources.list.d/caddy-stable.list
if ! need caddy; then
  # The repo is registered BEFORE caddy is installed — Ubuntu's own package installs
  # fine and would silently win otherwise, and it is too old for {client_ip} and
  # trusted proxies (needs >= 2.7; checked again below).
  apt-get update
  apt-get install -y caddy
fi
# {client_ip} and the trusted-proxies parser need Caddy >= 2.7 — a distro package that
# already satisfied `need caddy` could be older, and an older Caddy answers every
# request with an empty client_ip header (every visitor then shares one throttle key).
caddy_ver=$(caddy version 2>/dev/null | sed -n 's/^v\([0-9]*\.[0-9]*\).*/\1/p')
case "$caddy_ver" in
  ''|1.*|2.[0-6])
    echo "error: caddy ${caddy_ver:-unknown} < 2.7 — the trusted-proxy/Caddyfile config needs it" >&2
    exit 2 ;;
esac

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
# The env file's values feed this script's own references — SMON_DATABASE_URL for
# --init, SMON_LISTEN for the livez/smoke probes below. Sourced without exporting:
# they stay shell variables, so no child (systemctl, curl, the smoke run) inherits
# the admin token (S2 review; the earlier subshell hid SMON_LISTEN from the probes).
. "$PREFIX/env"
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
if [[ -n "$ORIGIN_CERT" ]]; then install -o root -g caddy -m 0640 "$ORIGIN_CERT" /etc/caddy/origin.pem; fi
if [[ -n "$ORIGIN_KEY" ]]; then install -o root -g caddy -m 0640 "$ORIGIN_KEY" /etc/caddy/origin.key; fi
if [[ ! -s /etc/caddy/origin.pem || ! -s /etc/caddy/origin.key ]]; then
  echo "Paste the Cloudflare ORIGIN CERTIFICATE (CREATE.md 7), then Ctrl-D:"
  cat >/etc/caddy/origin.pem
  echo "Paste the ORIGIN PRIVATE KEY, then Ctrl-D:"
  cat >/etc/caddy/origin.key
  chown root:caddy /etc/caddy/origin.pem /etc/caddy/origin.key
  chmod 0640 /etc/caddy/origin.pem /etc/caddy/origin.key
fi
if [[ ! -s /etc/caddy/cloudflare-origin-pull-ca.pem ]]; then
  # Pinned by whole-file sha256 (S2 review): this CA decides whose client
  # certificate passes Authenticated Origin Pulls. Hashing only the first
  # certificate would let a substituted file append its own CA after it — Caddy
  # trusts every certificate in the file, so the pin covers every byte.
  aop_ca=$(mktemp)
  curl -fsSo "$aop_ca" \
    https://developers.cloudflare.com/ssl/static/authenticated_origin_pull_ca.pem
  got=$(sha256sum "$aop_ca" | cut -d' ' -f1 | tr '[:lower:]' '[:upper:]')
  want=FA7511982D758634477A48DC6375F8FC8D5FED9FDB43235BC3923887FDEA0597
  [[ "$got" == "$want" ]] \
    || { echo "error: CF origin-pull CA sha256 $got, expected $want" >&2; rm -f "$aop_ca"; exit 2; }
  install -m 0644 "$aop_ca" /etc/caddy/cloudflare-origin-pull-ca.pem
  rm -f "$aop_ca"
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
  if [[ -n "$R2_SECRET" ]]; then
    install -m 0400 "$R2_SECRET" "$PREFIX/credentials/s3_secret_key"
    install -m 0644 "$R2_ENV" "$PREFIX/credentials/r2.env"
  else
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
fi

step "first off-box backup"
# The exit gate wants a backup less than a day old — take one now, through the real path
# (the server's own /api/backup → age → rclone), while the credentials are fresh. If the
# credentials are wrong the failure lands here, loudly, not at the next timer tick.
for _ in $(seq 1 30); do
  curl -fsS --max-time 2 "http://${SMON_LISTEN:-127.0.0.1:58400}/livez" >/dev/null 2>&1 && break
  sleep 1
done
SMON_LOCAL_URL="http://${SMON_LISTEN:-127.0.0.1:58400}" smon-backup run

step "smoke"
"$here/smoke.sh"
echo
echo "install done. Next: DEPLOY.md step 7's CF-only 443 check from your own machine,"
echo "step 8 (the first monitors), step 9 (the restore drill), step 10 (the outside checker)."
