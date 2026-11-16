# Deploy runbook — Service Monitor

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

```sh
scripts/deploy.sh --prefix /opt/smon            # builds --release, installs, links
$EDITOR /opt/smon/env                            # at minimum SMON_DATABASE_URL, SMON_STATUS_DIR

# systemd
sudo cp deploy/monitor-server.service /etc/systemd/system/
sudo systemctl enable --now monitor-server

# macOS launchd
sudo cp deploy/com.shirase.smon.plist /Library/LaunchDaemons/
sudo launchctl load /Library/LaunchDaemons/com.shirase.smon.plist
```

If the unit fails to start, read its log first: an invalid or blank `SMON_*` value stops
the boot with every problem listed (`invalid configuration (N problem(s)): …`) — the
server never runs on a silently substituted default.

Verify: `curl -sf $SMON_LISTEN/readyz` → `{"status":"ready"}` and
`curl -sf $SMON_LISTEN/version` → the expected release. A 503 names its cause in
`faults` (D-011): `store writes failing (N consecutive)` means the write path is broken
(disk, perms) — fix the cause rather than restart-looping; `writer exited|panicked` or
`coordinator …` means the monitor is not observing (statuses read `unknown`) — the log
has the task's error, then restart; `degraded` with a deliverer, snapshot or retention
fault means checks still run but that duty has stopped.

## Upgrade

```sh
scripts/deploy.sh --prefix /opt/smon            # new release dir, repoints current
sudo systemctl restart monitor-server           # or: launchctl kickstart -k system/com.shirase.smon
curl -sf .../readyz && curl -sf .../version     # the version must match the new release
```

Boot rebuilds incidents and streaks from the store — an upgrade mid-incident resumes
`down`, never fabricates a recovery.

## Rollback

```sh
ln -sfn /opt/smon/releases/<previous> /opt/smon/current
sudo systemctl restart monitor-server
curl -sf .../readyz
```

Forward-only migrations: a rollback across a schema change means the old binary reads
a newer schema. SQLite's `user_version` guards that — an old binary on a too-new schema
fails the migration check at boot and stays down, which is the honest failure.

## Backup / restore

```sh
curl -X POST .../api/backup -d '{"path":"/opt/smon/data/backups/monitor.db"}'
# restore: stop, copy the snapshot over data/monitor.db (remove -wal/-shm), start
scripts/backup-drill.sh   # the drill that proves it — run it before trusting it
```

## Watch the monitor

`status.json` declares `expires_at`; serve it from a separate static host and run
`fixtures/status-check.mjs` on a third. An expired beat means *monitoring* is
unavailable — never "all targets down".
