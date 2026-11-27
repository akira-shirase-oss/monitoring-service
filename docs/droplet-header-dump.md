# Droplet header dump — read-only verification

Owed evidence for "deployed off the target host": the raw headers and runtime state of
the deployed service, captured as artifacts. Run on the droplet as root except where
marked. **Every command is read-only**: `GET`/`HEAD` requests, `systemctl show/status`,
`sqlite3 -readonly`, `stat`, `df`, `grep` on configs. Nothing prints a secret —
`/opt/smon/env` is never read here, and the one marked-optional login probe uses a
deliberately wrong token (it increments the failed-login throttle by one request;
nothing is written).

Substitute the real domain for `monitor.example.com` (smoke.sh uses `SMOKE_DOMAIN`).

## 1. Through Cloudflare — the public face

```bash
domain=monitor.example.com
curl -sSI "https://$domain/"
```

Expected: `HTTP/2 200` · `server: cloudflare` · `content-security-policy: default-src
'self'; script-src 'self' 'sha256-…' [one hash per inline bootstrap script];
style-src 'self' 'unsafe-inline'; img-src 'self' data:; font-src 'self' data:;
connect-src 'self'; object-src 'none'; base-uri 'none'; form-action 'self';
frame-ancestors 'none'` · `x-content-type-options: nosniff` · `x-frame-options: DENY` ·
`referrer-policy: same-origin` · no `set-cookie`, no `x-powered-by`, no stack text.

```bash
curl -sSI "https://$domain/livez"
```

Expected: `HTTP/2 200` · `content-type: application/json…` · the same security headers
(middleware wraps every response) · no `set-cookie`.

```bash
curl -sSI "https://$domain/api/monitors"
```

Expected: `401` — never `200`, never a redirect — and the 401 *still* carries the
security headers (proves the layer wraps denials, not just successes).

```bash
curl -sSI "https://$domain/s/definitely-not-a-page/status.json"
```

Expected: `404` — never `401`, never an auth challenge. A public status slug that
doesn't exist is a plain miss.

```bash
curl -sS --max-time 10 "https://$domain/livez" | head -c 200; echo
```

Expected: the JSON liveness body — `{"status":"ok"}`-shaped, no HTML, no error text
from the proxy.

## 2. TLS — the origin serves only Cloudflare

```bash
echo | openssl s_client -connect "$domain":443 -servername "$domain" 2>/dev/null \
  | openssl x509 -noout -issuer -dates -fingerprint -sha256
```

Expected: issuer `CN=WE1`/Cloudflare-edge or Let's Encrypt E-chain per the DNS setup ·
`notAfter` in the future · the fingerprint is a public value (certs are public —
not a secret).

```bash
curl -sSI --max-time 5 "https://$(curl -sS ifconfig.me 2>/dev/null || dig +short "$domain" A | head -1)/" || echo refused
```

**Run from any machine that is not the droplet.** Expected: connection refused,
timeout, or a TLS handshake failure — the origin's Caddy/AOP answers Cloudflare
clients only (DEPLOY.md step 10's claim, observed from outside).

## 3. The service itself, on loopback

```bash
curl -sS http://127.0.0.1:58400/readyz
```

Expected: `{"status":"ready"}` (or `"degraded"` with named `faults` if a supporting
task is down — never `"ok"` under a dead writer). Never HTML, never a stack trace.

```bash
curl -sS http://127.0.0.1:58400/version
readlink /opt/smon/current
```

Expected: `{"version":"…","schema":N}` — the version string names the release
`readlink` points at; `schema` equals the migration count in
`crates/monitor-db/migrations.sha256` on the deployed commit.

```bash
# OPTIONAL — one unauthenticated request; increments the throttle by one, writes nothing.
curl -sS -D - -o /dev/null --max-time 10 -X POST "https://$domain/api/login" \
  -H 'content-type: application/json' -d '{"token":"header-dump-probe"}'
```

Expected: `401` with the uniform `{"error":…}` envelope and **no `set-cookie`** —
a server that emits a session cookie on a failed login is broken. (A real
`set-cookie` can't be observed without the admin token, which stays in `/opt/smon/env`;
the emitted shape — `__Host-smon_session; Secure; HttpOnly; SameSite=Strict` — is
pinned by `tests/api.rs` and by `SMON_COOKIE_SECURE=always` in the droplet env.)

## 4. The edge config — D-023's keep-alive bound, on the wire's terms

```bash
grep -n -A3 'transport http' /etc/caddy/Caddyfile
```

Expected: `keepalive 5s` inside `transport http` — the upstream idle keep-alive is
strictly below the server's 10 s header-read/idle bound (D-023: the peer side
expires first; the deploy_scripts test pins the ordering claim, this confirms the
deployed file carries it).

```bash
grep -n 'reverse_proxy\|import\|header' /etc/caddy/Caddyfile | head -20
```

Expected: a single `reverse_proxy 127.0.0.1:58400` (or the configured listen) — no
extra `header` directives inventing security headers at the edge (the app emits them).

## 5. Runtime state — unit, store, migrations, disk

```bash
systemctl show monitor-server -p ActiveState,SubState,ExecMainStatus,NRestarts,MemoryCurrent
```

Expected: `ActiveState=active`, `SubState=running`, `ExecMainStatus=0`, a low
`NRestarts` (nonzero is not itself a failure — check `journalctl` below for why),
`MemoryCurrent` in the tens of MiB (the soak baseline held ~18 MiB; a number an
order of magnitude higher wants an explanation, not a shrug).

```bash
journalctl -u monitor-server --since "-24h" -p err --no-pager | wc -l
journalctl -u monitor-server --since "-24h" -p warning --no-pager | tail -20
```

Expected: 0 error lines in 24 h on a quiet deployment (any nonzero count → read the
lines, they are evidence); warnings should be named conditions (throttle refusals,
capacity) never panics or unwrap text.

```bash
sqlite3 -readonly /opt/smon/data/monitor.db \
  "SELECT version, success, length(checksum) FROM _sqlx_migrations ORDER BY version;"
```

Expected: one row per migration `1..N` (N = the deployed commit's manifest count,
currently 20), every `success=1`, every checksum length `48` (sqlx's SHA-384). The
pre-deploy check from DEPLOY.md: version `18` specifically must read `success=1`
with the committed SHA-384 — a DB that applied the in-place-edited 0018 shows a
checksum that won't match the file and must be repaired before any upgrade.

```bash
stat -c '%a %U:%G %n' /opt/smon/env /opt/smon/data /opt/smon/data/monitor.db
```

Expected: `600 root:smon` (or `600 smon:smon`) on `env` — never world-readable;
`700`/`750` on `data`; `600` on `monitor.db`. The store holds token digests and
monitor URLs — owner-only is the deployed contract.

```bash
df -h /opt/smon/data; du -sh /opt/smon/data/monitor.db /opt/smon/data/backups 2>/dev/null
```

Expected: `monitor.db` comfortably under `SMON_MAX_DB_BYTES` (2 GiB) — the
24 h salvaged run reached ~150 MB at 576k checks; backups present and recent
(`smon-backup latest` prints the newest off-box snapshot's age; smoke.sh fails
past 26 h).

## 6. The snapshot heartbeat

```bash
stat -c '%y %n' /opt/smon/status/status.json; head -c 300 /opt/smon/status/status.json; echo
```

Expected: mtime within the last beat interval (seconds, not minutes), and the JSON
carries `expires_at` a beat or two in the future — the independent checker's only
verdict input. A stale or missing `expires_at` means the snapshot task stopped.

## What "pass" looks like

Every curl answers the status line printed, every expected header present verbatim,
no `set-cookie` anywhere unauthed, TLS valid through Cloudflare only, the unit quiet,
migrations 1..N all `success=1` with 48-byte checksums, and the snapshot fresh.
Paste the session output into the run record — the artifact is the point.
