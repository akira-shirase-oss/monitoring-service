# Security review — first deploy candidate

Reviewed 2026-11-18 (devin-smon worktree). Scope: the management API/CLI, probe path,
alert channels, dashboard session model, filesystem writes, deploy surface. This file is
the attacker-review record: every finding gets a severity, a status, and where it is
pinned. It stays current — amend it when a finding is fixed or a new one is found.

## Findings

| # | Finding | Severity | Status |
|---|---|---|---|
| F-1 | `POST /api/backup` wrote any absolute path the service user could write — a write-anywhere primitive if the admin token leaked | High | **Fixed** — D-019: `SMON_BACKUP_DIR` confinement, server-drawn `monitor-<UTC>-<rand>.db` names, any client path refused 422, `create_new`+0600, symlink refused, dir created 0700 at boot |
| F-2 | lettre 0.11.23: STARTTLS reply injection (RUSTSEC-2026-0560) and missing dot-stuffing after bare CR/LF (RUSTSEC-2026-0561) | Medium | **Fixed** — lettre 0.11.24; wire-level proof in `tests/email.rs`; `smtps://` (implicit TLS, no cleartext preamble) is the documented default |
| F-3 | Webhook target URL is not SSRF-guarded — an operator-set `SMON_WEBHOOK_URL` may dial anywhere, incl. internal addresses | Medium | **Accepted** — D-017: it is operator configuration, not user input; hardening is P0 **before** multi-tenant/customer webhooks. Recorded here so it can't be forgotten |
| F-4 | No general per-IP read rate limit on `/api/*` | Low | **Bounded** — auth-gated behind Caddy; credential attempts are throttled per address (`throttle.rs`), bodies ≤16 KiB, 10 s deadlines, connection cap + write-stall timeouts. A flood of *authenticated* reads is an operator problem, not an exposure |
| F-5 | Cookie `Secure` depends on the proxy declaring TLS (`X-Forwarded-Proto`) | Medium | **Deploy requirement** — behind Caddy set `SMON_TRUSTED_PROXY` (client attribution) and `SMON_COOKIE_SECURE=always`. The droplet package carries both |

## What the review found already strong

- **Probe path:** DNS answers validated inside the resolver; ≤5 revalidated redirects;
  literal private/local targets refused at registration; loopback only under
  `SMON_PROFILE=lab`; probes never follow `*_PROXY` envs (D-013, RUNBOOK egress rule).
- **Auth:** bearer compared by sha256 digest; session rows hold a token-bound digest of
  the cookie id (rotation ends every session, a stolen row is useless); idle + absolute
  expiry; per-address credential throttle before comparison; Origin≡Host CSRF on every
  credential; tokenless mode binds loopback only.
- **Honesty:** store-write failure → `unavailable`, not green; an observing-task exit
  stops serving and exits non-zero (D-011); a dead monitor is "monitoring unavailable",
  never "all targets down".
- **Messages:** monitor names are flattened (`one_line`) into Subject/body — header and
  SMTP-command injection proven closed on the wire (`tests/email.rs`), now doubly
  covered by lettre 0.11.24's own normalization + stuffing.
- **Files:** store, WAL, snapshots all 0600; `SMON_BACKUP_DIR` 0700; status snapshot is
  the only intentionally public file.
- **Restore hygiene:** `-wal`/`-shm` deleted before restore (RUSTSEC-2026-0459 —
  `deny.toml` carries the dated assessment).

## Residual risk (accepted, on record)

- Admin token is the single root credential; a leak reaches every management route —
  which is exactly why F-1 was fixed rather than documented. Rotate by editing `env` +
  restart; all sessions end at once.
- Snapshot files contain monitored URLs and incident history — kept owner-only on box,
  age-encrypted off box (droplet plan).
- `SMON_PROFILE=lab` disables target refusal — a deploy setting of last resort, never
  for production (the deploy package never sets it).
