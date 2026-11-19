# Deploying Service Monitor to its own droplet

The monitor's whole job is watching other things; running it in Toronto beside Colophon and
Durable Runner would mean a regional failure lies about all three. It lives on a 2 GB
DigitalOcean droplet in **SFO3**, behind Cloudflare at `https://monitor.example.com` —
a placeholder name; the real hostname lives only in the maintainer's environment.

**Before you start:** everything in [CREATE.md](CREATE.md) exists. Secrets are typed or pasted
only on the droplet, into root-only files; nothing secret goes into chat, an issue or git.
`deploy/RUNBOOK.md` is the generic operator runbook (env reference, upgrade/rollback, the
late-volume and full-store cases); this file is the concrete SFO3 install.

**Speed targets** (the first-deploy gate records the actuals in `deploy/runbooks/install-feedback.md`):

| Operation | Target |
|---|---|
| Fresh install (steps 1–11, operator time) | ≤ 45 min |
| Upgrade (repoint `current` + restart) | ≤ 2 min wall clock, ≤ 30 s serving gap (the SIGTERM drain bounds it: 12 s HTTP + 15 s engine) |
| Rollback | ≤ 60 s (repoint + restart; only across releases with no new migration) |
| `smoke.sh` | ≤ 60 s |
| Restore drill (step 12) | ≤ 15 min |

**Exit gate:** `smoke.sh` green, a restore drill from the R2 copy recorded (step 12), idle
memory recorded, the first monitors registered (step 13), and the outside checker green
(step 14).

The commands run on the droplet as root unless they say otherwise. `$CHECKOUT` is a copy of
this repository on the droplet (step 2).

## 1. The host

```sh
ssh root@<droplet-ip>
apt-get update && apt-get -y upgrade
apt-get install -y ufw jq curl ca-certificates unattended-upgrades age rclone sqlite3
dpkg-reconfigure -f noninteractive unattended-upgrades
timedatectl set-timezone UTC

fallocate -l 1G /swapfile && chmod 600 /swapfile && mkswap /swapfile && swapon /swapfile
echo '/swapfile none swap sw 0 0' >>/etc/fstab

# Firewall: SSH from you, 443 from Cloudflare only (step 7 adds those ranges), nothing else.
ufw default deny incoming && ufw default allow outgoing
ufw allow from <your-ip> to any port 22 proto tcp
ufw --force enable
```

Keep the DigitalOcean cloud firewall from CREATE.md as well: two layers, either one enough.
(AOP — Cloudflare's Authenticated Origin Pulls, step 7 — adds a third, at the TLS layer.)

Mount the data volume at `/opt/smon/data` (CREATE.md 3) — its fstab entry waits for the
device (`nofail,x-systemd.device-timeout=10min`); the unit's `RequiresMountsFor` keeps the
order, and D-015 refuses a bare mountpoint anyway.

## 2. The binaries and the dashboard

Build on a build host (a 2 GB droplet compiles Rust, slowly; the workspace builds releases),
copy `target/release/{monitor-server,monitor}` and `apps/dashboard/build` up, or use
`scripts/deploy.sh` from a checkout:

```sh
git clone --depth 1 https://github.com/akira-shirase-oss/service-monitor /opt/smon-src
CHECKOUT=/opt/smon-src
# On the build host: cargo build --release && npm --prefix apps/dashboard ci && npm --prefix apps/dashboard run build
$CHECKOUT/scripts/deploy.sh --prefix /opt/smon    # releases/<ver>/, repoints current
monitor-server --version && monitor --version
```

## 3. The user, the store, the env file

```sh
useradd --system --home-dir /opt/smon --no-create-home --shell /usr/sbin/nologin smon
install -d -o smon -g smon -m 0700 /opt/smon/data /opt/smon/status /opt/smon/data/backups
install -d -m 0700 /opt/smon/credentials

install -m 0644 $CHECKOUT/deploy/droplet/smon.env.template /opt/smon/env
# Generate the admin token INTO the env file — it is never seen anywhere else:
printf 'SMON_ADMIN_TOKEN=%s\n' "$(openssl rand -hex 32)" >>/opt/smon/env
nano /opt/smon/env                       # webhook/SMTP if used; the rest is shipped
chown root:root /opt/smon/env && chmod 600 /opt/smon/env
```

`SMON_BACKUP_DIR=/opt/smon/data/backups` is already in the template — the only directory
`/api/backup` may write (D-019), created 0700 at boot.

## 4. The store (once — D-015)

```sh
set -a; . /opt/smon/env; set +a
sudo -u smon /opt/smon/current/bin/monitor-server --init
```

A normal start later refuses a missing or empty store: a volume that isn't mounted yet can
never be mistaken for a new install.

## 5. The service

```sh
install -m 0644 $CHECKOUT/deploy/monitor-server.service /etc/systemd/system/
systemctl daemon-reload && systemctl enable --now monitor-server
journalctl -u monitor-server -n 30 --no-pager
curl -sf http://127.0.0.1:58400/readyz | jq .
```

## 6. Caddy, the origin certificate, AOP, Cloudflare's ranges

```sh
curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/gpg.key' | gpg --dearmor -o /usr/share/keyrings/caddy-stable-archive-keyring.gpg
curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/debian.deb.txt' >/etc/apt/sources.list.d/caddy-stable.list
apt-get update && apt-get install -y caddy

# Cloudflare's Origin Certificate (CREATE.md 7): paste each, Ctrl-D.
umask 077; cat >/etc/caddy/origin.pem; cat >/etc/caddy/origin.key
chown root:caddy /etc/caddy/origin.pem /etc/caddy/origin.key && chmod 0640 /etc/caddy/origin.pem /etc/caddy/origin.key

# AOP: Cloudflare's published origin-pull CA — the origin then requires the TLS client to be Cloudflare.
curl -fsSo /etc/caddy/cloudflare-origin-pull-ca.pem https://developers.cloudflare.com/ssl/static/authenticated_origin_pull_ca.pem
chmod 0644 /etc/caddy/cloudflare-origin-pull-ca.pem

install -m 0644 $CHECKOUT/deploy/droplet/Caddyfile /etc/caddy/Caddyfile
$CHECKOUT/deploy/droplet/cloudflare-ips.sh       # Caddy's trusted ranges + ufw 443-from-Cloudflare
caddy validate --config /etc/caddy/Caddyfile && systemctl reload caddy
systemctl restart monitor-server
```

`SMON_TRUSTED_PROXY=127.0.0.1` (in the template) is right here: Caddy is the only proxy smon
sees, and Caddy trusts Cloudflare's ranges, so `X-Forwarded-For` already carries the visitor.
`SMON_COOKIE_SECURE=always` because every dashboard request arrives over TLS.

## 7. Sign in once

`https://monitor.example.com` asks for the admin token — it is in `/opt/smon/env` (root-only).
The session cookie it earns is HttpOnly, SameSite=Strict, Secure.

## 8. Alert channels (optional)

`SMON_WEBHOOK_URL` + `SMON_WEBHOOK_SECRET` and/or the three `SMON_SMTP_*` go in `/opt/smon/env`
(prefer `smtps://` — implicit TLS; `?tls=required` only where the provider offers no implicit
TLS). Without a channel, incident events stay `pending` in the outbox — visible, never dropped.

## 9. Backups: local snapshot, age, R2 with a 7-day lock

Daily: `monitor backup` draws a `VACUUM INTO` snapshot inside `SMON_BACKUP_DIR` (D-019), the
script encrypts it with age to the droplet's recipient, uploads to `smon-backups/snapshots/`,
then prunes remotely (14 daily + 8 weekly kept) and locally (3 newest kept). The bucket's 7-day
object lock means nothing can delete or rewrite a week of snapshots — including root on a
ransomed droplet. Everything pruned is always older than the lock window.

```sh
umask 077; cat >/opt/smon/credentials/s3_secret_key      # R2 secret, Ctrl-D
chmod 0400 /opt/smon/credentials/s3_secret_key
install -d -m 0755 /opt/smon/credentials
printf 'SMON_R2_ENDPOINT=https://<r2-account-id>.r2.cloudflarestorage.com\nSMON_R2_ACCESS_KEY=<r2-access-key-id>\n' \
  >/opt/smon/credentials/r2.env && chmod 0644 /opt/smon/credentials/r2.env

age-keygen -o /opt/smon/backup-age-identity 2>/dev/null && chmod 0400 /opt/smon/backup-age-identity
age-keygen -y /opt/smon/backup-age-identity >/opt/smon/backup-age-recipient
cat /opt/smon/backup-age-identity    # copy the AGE-SECRET-KEY-… line offline, then clear the screen

install -m 0755 $CHECKOUT/deploy/droplet/smon-backup.sh /usr/local/bin/smon-backup
install -m 0644 $CHECKOUT/deploy/droplet/smon-backup.service /etc/systemd/system/
install -m 0644 $CHECKOUT/deploy/droplet/smon-backup.timer /etc/systemd/system/
systemctl daemon-reload && systemctl enable --now smon-backup.timer
systemctl start smon-backup && journalctl -u smon-backup -n 10 --no-pager
smon-backup latest           # the newest upload and its age in hours
```

## 10. The smoke check

```sh
$CHECKOUT/deploy/droplet/smoke.sh
```

Then from **your own machine** (not the droplet), check only Cloudflare reaches 443:

```sh
curl -sS --max-time 5 --resolve monitor.example.com:443:<droplet-ip> https://monitor.example.com/livez \
  && echo "FAIL: the droplet answers directly" || echo "ok: only Cloudflare reaches 443"
```

(A passing direct curl means the ufw rule didn't land; AOP would still refuse it at TLS —
check both.) Record the smoke output, including idle memory, in `deploy/runbooks/install-feedback.md`.

## 11. The first monitors

Register through the dashboard or CLI (`monitor monitors add`). The first set, per the
deploy decision — targets are placeholders for the real hostnames:

| Name | URL | Interval | Why |
|---|---|---|---|
| colophon-readyz | `https://app.colophon.404human.dev/readyz` | 30 s | the CMS's own readiness verdict |
| colophon-landing | `https://colophon.404human.dev/` | 60 s | the public landing page |
| colophon-docs | `https://docs.colophon.404human.dev/` | 60 s | the docs site |
| runner-health | `https://<durable-runner>/health` | 60 s | Durable Runner, once it is deployed |

Each registration is checked by the SSRF guard — a name that resolves to a private or
loopback address is refused at registration, which on this droplet means exactly the
targets you intend.

## 12. The restore drill (exit gate)

Prove the R2 copy is a restore, on any machine with `age` and `sqlite3` (your laptop is fine):

```sh
rclone copy r2:smon-backups/snapshots/<newest>.db.age .    # or download it in the dashboard
age --decrypt -i <your-identity-file> -o drill.db <newest>.db.age
sqlite3 drill.db "SELECT COUNT(*) FROM monitors; SELECT COUNT(*) FROM checks; SELECT COUNT(*) FROM incidents WHERE resolved_at IS NULL;"
```

The counts should match what the dashboard shows at the snapshot's time. For a full service
drill, `scripts/backup-drill.sh` (own ports, safe beside the live droplet) proves the whole
backup→destroy→restore path keeps an open incident. Record the numbers in
`deploy/runbooks/install-feedback.md`.

## 13. The outside checker

`deploy/github/outside-check.yml` is the proposed workflow: every 5 minutes a GitHub runner —
outside both droplets — curls the public endpoints and republishes the status page to
Cloudflare Pages from smon's `status.json`. Its Cloudflare token lives in GitHub secrets,
never on the droplet. Move it to `.github/workflows/` and set the secrets to activate.
**Scheduled Actions runs can be delayed under load** — which is exactly what `status.json`'s
`expires_at` is for: a late beat shows "monitoring unavailable" on the page instead of lying.

## Upgrading

`scripts/deploy.sh --prefix /opt/smon` (new release dir, repoints `current`), `systemctl
restart monitor-server`, then `smoke.sh`. A release that adds migrations can't be rolled back
by repointing — `monitor backup` first (RUNBOOK). Boot rebuilds incidents and streaks from
the store; a gap longer than a check interval is committed as `missed` rows, never hidden.

## Rollback

```sh
ln -sfn /opt/smon/releases/<previous> /opt/smon/current
systemctl restart monitor-server && curl -sf http://127.0.0.1:58400/readyz
```

Only across releases with no new migration — the honest failure is that an older binary
refuses a newer schema (RUNBOOK, Rollback).

## When something is wrong

| Symptom | Look at |
|---|---|
| `monitor-server` won't start | `journalctl -u monitor-server`: an invalid `SMON_*` names itself |
| `no store at …` | the data volume isn't mounted (`findmnt /opt/smon/data`) — never `--init` over it |
| `/readyz` `store writes failing` | disk full or `SMON_MAX_DB_BYTES` cap (RUNBOOK has both) |
| Cloudflare 52x | `systemctl status caddy`, the origin cert files, `ufw status` (443 from Cloudflare) |
| TLS handshake failure direct to origin | expected — AOP requires the client to be Cloudflare; go through the name |
| Every client looks like one address | `SMON_TRUSTED_PROXY` unset; Caddy's trusted ranges stale → `cloudflare-ips.sh` |
| `monitor backup` 404 `backup_disabled` | `SMON_BACKUP_DIR` unset in `/opt/smon/env` |
| `smon-backup` fails at R2 | `credentials/r2.env` ids and `credentials/s3_secret_key` |
