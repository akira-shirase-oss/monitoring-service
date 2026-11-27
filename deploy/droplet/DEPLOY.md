# Deploying Service Monitor to its own droplet

The monitor's whole job is watching other things; running it in Toronto beside Colophon and
Durable Runner would mean a regional failure lies about all three. It lives on a 2 GB
DigitalOcean droplet in **SFO3**, behind Cloudflare at `https://monitor.example.com` —
a placeholder name; the real hostname lives only in the maintainer's environment.

**Before you start:** everything in [CREATE.md](CREATE.md) exists. Secrets are typed or pasted
only on the droplet, into root-only files; nothing secret goes into chat, an issue or git.
`deploy/RUNBOOK.md` is the generic operator runbook (env reference, upgrade/rollback, the
late-volume and full-store cases); this file is the concrete SFO3 install.

**Speed targets** — the same as the other products; the first-deploy gate records the
actuals in `deploy/runbooks/install-feedback.md`:

| Operation | Target |
|---|---|
| Fresh install | ≈ 15 min, through one idempotent `install.sh` (prompts only for secrets) |
| Upgrade | < 2 min wall clock, < 5 s of request unavailability — Caddy's `lb_try_duration 15s` / `lb_try_interval 250ms` holds in-flight requests while the service restarts |
| Rollback | < 1 min (schema-guard + atomic repoint + restart; refused — naming both versions — when the database's migrations are newer than the target binary's, see `schema-guard.sh`) |
| `smoke.sh` | ≤ 60 s |
| Restore drill (step 12) | ≤ 15 min |

Honest accounting of the restart: the HTTP gap a caller sees is < 5 s, but the engine stops
*observing* for the restart — every check slot whose deadline fell inside it is committed as
a `missed` row on boot (D-014). At a 30 s interval a typical restart misses 0–1 slots per
monitor; the monitor records its own downtime exactly the way it records a target's — the
rows are evidence, not an error, and an open incident is never closed by missed slots.

**Exit gate:** `smoke.sh` green, a restore drill from the R2 copy recorded (step 9), idle
memory recorded, the first monitors registered (step 8), and the outside checker green
(step 10).

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

## 2. A checkout and the release payload

```sh
git clone --depth 1 https://github.com/akira-shirase-oss/service-monitor /opt/smon-src
CHECKOUT=/opt/smon-src
```

Build the binaries + dashboard on a build host (a 2 GB droplet compiles Rust, slowly) and
stage a release dir in `scripts/deploy.sh`'s layout — `bin/monitor-server`, `bin/monitor`,
`dashboard/` — then copy it up:

```sh
# on the build host, from the checkout:
cargo build --release -p monitor-server -p monitor-cli
npm --prefix apps/dashboard ci && npm --prefix apps/dashboard run build
mkdir -p release/bin && cp target/release/monitor-server target/release/monitor release/bin/
cp -R apps/dashboard/build release/dashboard
rsync -a release/ root@<droplet-ip>:/opt/release-candidate/
```

## 3. install.sh — the whole install, one command

```sh
$CHECKOUT/deploy/droplet/install.sh --release-dir /opt/release-candidate
```

≈ 15 minutes, idempotent — re-runnable top to bottom, every step checks before it does —
and under `umask 077`. It installs the packages, creates the `smon` user and the 0700
directory layout, writes `/opt/smon/env` from `smon.env.template` (generating
`SMON_ADMIN_TOKEN` into it — never echoed), installs the release under `releases/` and
points `current` at it, runs `monitor-server --init` once (D-015 — an existing store is
never re-initialised), installs and enables the units (server + `smon-backup.timer`),
installs Caddy + the Caddyfile, prompts for the origin certificate pair only if missing,
fetches Cloudflare's origin-pull CA (AOP), applies `cloudflare-ips.sh` (Caddy trusted
proxies + ufw 443-from-Cloudflare), generates the age identity if missing, prompts for the
R2 secret, takes the first off-box backup (`smon-backup run` — the exit gate's "a backup
exists" is evidence, not a wait for the first timer tick), and finishes with `smoke.sh`.
Secrets are prompted for with echo off; nothing secret lands anywhere but the root-only
files CREATE.md names.

Non-interactive installs (CI, replays) pass the same secrets without a prompt and never
as argv values — `ps` shows argv to every local user, so each flag takes a file path:
`--origin-cert FILE --origin-key FILE` install the origin pair, and `--r2-secret FILE
--r2-env FILE` together seed the R2 credentials (the env file carries
`SMON_R2_ENDPOINT`/`SMON_R2_ACCESS_KEY`). A partial set is refused. The deploy-check
workflow runs exactly this on a hosted VM (rclone stubbed to a filesystem remote — the
pipeline is real, the account is not), twice, for idempotence.

What it deliberately does **not** do: overwrite an existing `env`, re-initialise a store,
or weaken the file modes.

`SMON_TRUSTED_PROXY=127.0.0.1` (in the template) is right here: Caddy is the only proxy smon
sees, and Caddy attributes the visitor over Cloudflare's trusted ranges.
`SMON_COOKIE_SECURE=always` because every dashboard request arrives over TLS. A
plain `http://` deployment is **not** supported for sign-in — there is no
`SMON_COOKIE_SECURE=off` in any profile — so a self-hosted install must still put
TLS in front — a tunnel, a CA-signed cert, or mkcert on the LAN — rather than
serve the dashboard unencrypted.

## 4. Sign in once

`https://monitor.example.com` asks for the admin token — it is in `/opt/smon/env` (root-only).
The session cookie it earns is HttpOnly, SameSite=Strict, Secure.

## 5. Alert channels (optional)

`SMON_WEBHOOK_URL` + `SMON_WEBHOOK_SECRET` and/or the three `SMON_SMTP_*` go in `/opt/smon/env`
(prefer `smtps://` — implicit TLS; `?tls=required` only where the provider offers no implicit
TLS). Without a channel, incident events stay `pending` in the outbox — visible, never dropped.

## 6. Backups: local snapshot, age, R2 with a 7-day lock

`install.sh` already installed `smon-backup` and its timer, generated the age identity, and
prompted the R2 credentials into `/opt/smon/credentials/` (`s3_secret_key` 0400,
`r2.env` 0644 — the ids aren't secret). One thing only you can do: **copy the
`AGE-SECRET-KEY-…` line out of `/opt/smon/backup-age-identity` into a password manager or
onto paper** — the off-box backups are unreadable without it.

Daily: `monitor backup` draws a `VACUUM INTO` snapshot inside `SMON_BACKUP_DIR` (D-019), the
script encrypts it with age to the droplet's recipient, uploads to `smon-backups/snapshots/`,
then prunes remotely (14 daily + 8 weekly kept) and locally (3 newest kept). The bucket's
7-day object lock means nothing can delete or rewrite a week of snapshots — including root
on a ransomed droplet. Everything pruned is always older than the lock window; a refusal
inside it is logged `kept`, and anything else fails the run.

```sh
systemctl start smon-backup && journalctl -u smon-backup -n 10 --no-pager
smon-backup latest           # the newest upload and its age in hours
smon-backup prune --dry      # what retention would drop, without deleting
```

## 7. The smoke check

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

## 8. The first monitors

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

## 9. The restore drill (exit gate)

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

## 10. The outside checker

`deploy/github/outside-check.yml` is the proposed workflow: every 5 minutes a GitHub runner —
outside both droplets — curls the public endpoints and republishes the status page to
Cloudflare Pages from smon's `status.json`. Its Cloudflare token lives in GitHub secrets,
never on the droplet. Move it to `.github/workflows/` and set the secrets to activate.
**Scheduled Actions runs can be delayed under load** — which is exactly what `status.json`'s
`expires_at` is for: a late beat shows "monitoring unavailable" on the page instead of lying.

## Upgrading

`monitor backup` first if the release adds a migration (rollback can't repoint past it — see
below), then:

```sh
$CHECKOUT/deploy/droplet/upgrade.sh --release-dir /opt/release-candidate
# or a published artifact:
$CHECKOUT/deploy/droplet/upgrade.sh --version <v> --url <tarball> --sha256 <hex>
```

Stages under a unique name, verifies the download's SHA-256, runs `schema-guard.sh`
and the migration inspector **on the staged copy before anything under `releases/`
moves**, repoints `current` with an atomic rename, restarts, polls readiness,
runs the local smoke — and **rolls back automatically** on any post-switch failure. Keeps
the newest 3 release dirs. Serving gap < 5 s (Caddy holds requests); check slots inside the
restart are committed `missed` on boot — that's the monitor accounting for its own gap.

**Release names are unique.** The release's name comes from `--release-dir`'s basename
(or `--version`), and an upgrade refuses while still staging if that name already exists
under `/opt/smon/releases/` — a repeated `--release-dir /opt/release-candidate` computes
the same destination, and overwriting the live release before the guards ran is exactly
the failure the rule exists for. Give every candidate its own directory
(`/opt/release-candidate-<build>` or `--version`) and re-runs are safe by construction.

**Migration checksum before switching (read-only).** If the database ever ran a
branch/experiment build — anything other than a main-line release — a migration
file it applied may differ from what the candidate build ships, and sqlx refuses
to boot on the checksum mismatch *after* the switch. Check first; the query is
read-only:

```sh
dbpath=$(sed -n 's/^SMON_DATABASE_URL=sqlite://p' /opt/smon/env | tail -n 1)
sqlite3 -batch -readonly "$dbpath" \
  "SELECT version, description, success, hex(checksum) FROM _sqlx_migrations
   ORDER BY version DESC LIMIT 5;"
```

`inspect-migrations.sh` runs this comparison automatically inside `upgrade.sh`
before the atomic switch — against the candidate binary's own compiled-in
manifest (`--migration-manifest`), so no source checkout is needed on the
host — and a divergence, a `success = 0` row, or a query that cannot answer
aborts the upgrade rather than waving it through. `hex(checksum)` is what
`shasum -a 384` prints, so an applied row can also be compared by hand to the
candidate's manifest line (sqlx records the SHA-384 of the migration's SQL).
Any row whose version has no line in the manifest — or whose hex checksum
differs — means the database diverged from the release line: reconcile
`_sqlx_migrations` by hand before deploying, don't boot through it.

Version 18 is the historical edge: it was introduced and restored on a
branch before this release line carried it, so a database that last ran a
**pre-merge** build may hold a version-18 row written by a different 0018's
bytes. Once this release line has deployed, a version-18 row is ordinary —
do not "repair" a database merely for containing it. The tell is only a
version-18 row whose checksum differs from the candidate's
`0018_*.sql` file — divergence, not presence.

## Rollback

```sh
$CHECKOUT/deploy/droplet/rollback.sh            # the release before current
$CHECKOUT/deploy/droplet/rollback.sh --to <v>   # or a named release
```

The schema guard runs first: migrations are forward-only, so a store whose `_sqlx_migrations`
is newer than the target binary's `--db-version` is **refused — naming both versions — before
the switch**, and an unreadable database fails closed. That case is a restore-from-backup,
not a repoint (RUNBOOK, Rollback; the same lesson as Durable Runner's D-056).

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
