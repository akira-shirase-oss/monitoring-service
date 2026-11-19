# Deploy runbook — Service Monitor

This is the operator runbook: env reference, upgrade/rollback, backup/restore, and the
honest-failure cases. The concrete first production install — SFO3 droplet, Cloudflare,
Caddy with the origin cert + AOP, the R2+age backup timer, smoke and restore drill — is
[`deploy/droplet/`](droplet/DEPLOY.md), which defers to this file for the internals.

The monitor's whole job is watching other things; running it on the same box it watches
means a shared failure reports nothing — or worse, lies about both. Run it on its own
host, and host the public status page somewhere else again (`SMON_STATUS_DIR` is just a
directory — sync it to any static host; the page has no runtime dependency on the
monitor).

## Layout

```
/opt/smon/
  releases/<release>/bin/{monitor-server,monitor}   # versioned installs
  releases/<release>/dashboard/                     # built SPA, served same-origin
  current -> releases/<release>                     # repoint = upgrade or rollback
  env                                               # SMON_* (secrets live here, 0600)
  data/monitor.db                                   # the store — outside releases
  status/status.json                                # heartbeat the status host pulls
```

`current` is the only thing an upgrade touches; `data/` never lives under a release, so
rolling back the binary cannot roll back evidence.

## Install

**Exposure.** The listener closes a request whose header hasn't arrived within 10 s and an
idle keep-alive after 10 s, a connection whose response write makes no progress for
`SMON_WRITE_STALL_MS` (default 10 s — a client that never reads), and any connection after
1000 requests or 5 minutes; it serves at most `SMON_MAX_CONNECTIONS` (default 1024)
connections at once — the rest wait in the backlog, so a flood of idle sockets can't take
the file descriptors probes and SQLite need (`perf/connections.mjs` drills it). It speaks
plain HTTP: put a TLS proxy (Caddy) in front of anything reachable from the internet, and
set `SMON_TRUSTED_PROXY` to that proxy's address so failed credentials are throttled per
real client rather than all under the proxy's address.

**Direct egress is required.** Probes and alerts dial their targets and receivers directly
and ignore `HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY` (a proxied probe would bypass the SSRF
guard). On a network that only allows egress through a proxy, every probe fails `connect`
(every monitor opens an incident) and no alert can leave.

```sh
scripts/deploy.sh --prefix /opt/smon            # builds --release, installs, links
$EDITOR /opt/smon/env                            # at minimum SMON_DATABASE_URL, SMON_STATUS_DIR

# systemd — the unit runs as the unprivileged system user `smon`, never root
sudo useradd --system --home-dir /opt/smon --no-create-home --shell /usr/sbin/nologin smon
# Create the store once (D-015: a normal start refuses a missing or empty store, so a
# volume that isn't mounted yet can never be mistaken for a new install):
sudo sh -c 'set -a; . /opt/smon/env; set +a; exec /opt/smon/current/bin/monitor-server --init'
sudo chown -R smon:smon /opt/smon/data /opt/smon/status   # the only paths it writes
sudo chmod 700 /opt/smon/data   # store, -wal/-shm, logs, backups: nobody else reads or plants files
sudo chown root:root /opt/smon/env && sudo chmod 600 /opt/smon/env  # systemd reads it as root
sudo cp deploy/monitor-server.service /etc/systemd/system/
sudo systemctl daemon-reload && sudo systemctl enable --now monitor-server

# macOS launchd (development hosts; this plist still runs as root — add a UserName
# key for a dedicated account before using it anywhere that matters)
sudo cp deploy/com.shirase.smon.plist /Library/LaunchDaemons/
sudo launchctl load /Library/LaunchDaemons/com.shirase.smon.plist
```

If the unit fails to start, read its log first: an invalid or blank `SMON_*` value stops
the boot with every problem listed (`invalid configuration (N problem(s)): …`) — the
server never runs on a silently substituted default.

If the log says `no store at …`: the data volume is not mounted (check `findmnt
/opt/smon/data`), or this is a new install that skipped `--init`. Never run `--init` to
"fix" a store that should exist — mount the volume instead.

Verify: `curl -sf $SMON_LISTEN/readyz` → `{"status":"ready"}` and
`curl -sf $SMON_LISTEN/version` → the expected release. `/readyz` answers 200 with
`{"status":"degraded","faults":[…]}` when checks run but a supporting duty has stopped
(`monitor ping` exits 5), and 503 when the monitor is not observing. Either way it names
the cause in `faults` (D-011): `store writes failing (N consecutive)` means the write path is broken
(disk, perms) — fix the cause rather than restart-looping; `writer exited|panicked` or
`coordinator …` means the monitor is not observing (statuses read `unknown`) — the
process then writes one "monitoring unavailable" beat and exits non-zero on its own, and
the service manager restarts it (D-011; a coordinator stalled for 2 minutes does the
same); repeated restarts mean the log's task error needs fixing. Restarts back off from
15 s to 10 minutes (systemd ≥ 254), so a crash that clears (a dependency the process
itself retries, a store that comes back) is retried and a crash loop never hammers. On
older systemd (fixed 15 s) 10 starts within 30 minutes leave the unit `failed` (`start
request repeated too quickly`): fix the cause, then `sudo systemctl reset-failed
monitor-server && sudo systemctl start monitor-server`. `degraded` with a deliverer,
snapshot or retention fault means checks still run but that duty has stopped.

### The data volume attached late

*(This follows from `systemd.unit(5)`; not yet exercised on a systemd host.)*

The unit waits for the mounts of `data/` and `status/` (`RequiresMountsFor`). If the
volume's device has not appeared by its mount's `x-systemd.device-timeout` (90 s by
default), the mount job **fails**, and to systemd that is a *dependency failure* of
`monitor-server`, not a crash: `Restart=` does not apply, so the unit stays inactive — the
public heartbeat expires and the status page says "monitoring unavailable" — until it is
started by hand. Nothing retries it on its own. Once the volume is there:

```sh
findmnt /opt/smon/data || sudo mount /opt/smon/data   # the volume, not the bare mountpoint
sudo systemctl start monitor-server
```

To make this rarer, give the volume's fstab entry a longer wait (for example
`nofail,x-systemd.device-timeout=10min`); never remove `RequiresMountsFor` — D-015 would
refuse the bare mountpoint anyway, but the unit is what keeps the order right.

### The store is at its cap (`SMON_MAX_DB_BYTES`)

`/readyz` answers `store writes failing` and the log shows `database or disk is full`.
The retention pass (every `SMON_RETENTION_INTERVAL_S`, default hourly) can run at the
cap: it deletes expired rows before writing their daily summaries, and may use up to
256 pages (1 MiB at 4 KiB pages) past the cap for those summaries — in total, across all
passes, not per pass. Observation commits land again only once that pass has freed room for them —
which happens when the expired rows filled whole pages. If the store is full of
evidence that has not expired yet (or the expired slice is too thin to free a page),
retention cannot make room for new checks: raise `SMON_MAX_DB_BYTES` (or shorten the
D-012 horizons) in `env` and restart. A restart re-applies the new cap to every
connection.

## Upgrade

**Before upgrading to strict configuration** (invalid `SMON_*` values stop the boot): an
`env` written by an older `deploy.sh` had comments on the same line as some values. systemd
keeps those as part of the value, so the new boot refuses them. Move every comment onto its
own line first; this lists the offending lines:

```sh
grep -nE '^[[:space:]]*SMON_[A-Z_]+=.*[[:space:]]#' /opt/smon/env
```

**Upgrading an install that ran as root** (units from before the `smon` user). The new
unit sets `User=smon`; restarting without the account fails with `217/USER`, and without
the ownership change SQLite cannot write `data/`. Once, before the normal steps below:

```sh
sudo systemctl stop monitor-server
sudo useradd --system --home-dir /opt/smon --no-create-home --shell /usr/sbin/nologin smon
sudo chown -R smon:smon /opt/smon/data /opt/smon/status   # store, -wal/-shm, logs, snapshot
sudo chmod 700 /opt/smon/data && sudo chmod 600 /opt/smon/data/monitor.db*  # owner-only
sudo chown root:root /opt/smon/env && sudo chmod 600 /opt/smon/env
sudo cp deploy/monitor-server.service /etc/systemd/system/
sudo systemctl daemon-reload
```

Then check `env` against the strict parser (inline comments above; an admin token of at
least 32 bytes) and continue:

**Back up first.** A release that adds migrations can't be rolled back by repointing
`current` (see Rollback), so take a backup with the *running* release before deploying —
the server names the file inside `SMON_BACKUP_DIR` (D-019):

```sh
monitor backup        # prints the server-drawn name + size; or curl:
curl -X POST .../api/backup -H "authorization: Bearer $SMON_ADMIN_TOKEN" \
    -H 'content-type: application/json' -d '{}'
```

```sh
scripts/deploy.sh --prefix /opt/smon            # new release dir, repoints current
sudo systemctl restart monitor-server           # or: launchctl kickstart -k system/com.shirase.smon
curl -sf .../readyz && curl -sf .../version     # the version must match the new release
```

**Upgrading across the 2026-11-16 security release** (migrations 0008/0009): dashboard
sessions are dropped once — sign in again. After that, rotating `SMON_ADMIN_TOKEN` (edit
`env`, restart) ends every dashboard session, and sessions unused for
`SMON_SESSION_IDLE_S` (default 1 h) end on their own. Backup targets must be new paths.
The store, its `-wal` and `-shm` are made owner-only (0600) when the server opens them, and
logged when they weren't; make the directory private too, once:
`sudo chmod 700 /opt/smon/data && sudo chmod 600 /opt/smon/data/monitor.db*`.

Boot rebuilds incidents and streaks from the store — an upgrade mid-incident resumes
`down`, never fabricates a recovery. If the monitor was down for longer than a check
interval, boot records that gap (one `missed` row per monitor, D-014): statuses read
`unknown` until the first fresh probe, and failures from before the outage don't count
toward a new incident.

Stopping (`systemctl stop`, `launchctl kickstart -k`, `kill`) sends SIGTERM, which drains
like Ctrl-C: readiness goes false, the coordinator stops scheduling, open requests get up
to 12 s and the engine (in-flight probes, deliveries, queued commits) up to 15 s, then the
process exits 0. The log shows `shutting down` (with the signal), then `engine drained`.

## Rollback

```sh
ln -sfn /opt/smon/releases/<previous> /opt/smon/current
sudo systemctl restart monitor-server
curl -sf .../readyz
```

Repointing is enough only when the newer release added no migration. Migrations are
forward-only and recorded in sqlx's `_sqlx_migrations` table: an older binary on a store
a newer one migrated refuses to start ("migration N was previously applied but is missing
in the resolved migrations"), which is the honest failure — it never reads a schema it
doesn't know. To roll back across a migration (for example 0008/0009 of the 2026-11-16
security release), restore the backup taken before the upgrade: stop the server, copy that
backup over `data/monitor.db`, delete `monitor.db-wal`/`-shm`, repoint `current`, start.
Evidence written after the upgrade is lost; there is no downgrade migration.

## Backup / restore

`POST /api/backup` writes every snapshot inside `SMON_BACKUP_DIR` — an absolute path the
boot creates owner-only (0700), e.g. `/opt/smon/data/backups`. The client supplies nothing
but `{}`; the server draws the name (`monitor-<UTC>-<random>.db`, created `create_new`
and 0600) and answers `{"name", "bytes"}`. A body naming a path is refused with 422:
a client-chosen path was a write-anywhere primitive, so there is deliberately no way to
aim the write (D-019). With `SMON_BACKUP_DIR` unset the endpoint is off (404
`backup_disabled`). Snapshots hold monitored URLs and alert history — keep the directory
owner-only; `data/` (0700) is the right parent.

Restore only into a stopped server, and delete any `monitor.db-wal`/`-shm` beside the
restored file first: the bundled SQLite (`libsqlite3-sys` 0.37) is affected by
RUSTSEC-2026-0459, which needs a database opened together with a `-wal` that someone else
wrote (deny.toml records the assessment and the re-check date).

```sh
monitor backup                     # or curl -X POST .../api/backup -d '{}'
# restore: stop, copy the snapshot over data/monitor.db (remove -wal/-shm), start
scripts/backup-drill.sh   # the drill that proves it (own ports 58460/58461; safe beside a live monitor)
```

## Watch the monitor

`status.json` declares `expires_at`; serve it from a separate static host and run
`fixtures/status-check.mjs` on a third. An expired beat means *monitoring* is
unavailable — never "all targets down".
