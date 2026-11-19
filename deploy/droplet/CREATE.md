# What to create before the deploy

Everything here is created in your own accounts. **No secret is ever pasted into chat, an issue or
git**: each one is typed or pasted only on the droplet, into the file `DEPLOY.md` names, which only
root can read. Hostnames here are placeholders — the real names live in the maintainer's
environment (`monitor.example.com`, `status.example.com` below).

| # | Where | Create | Note for the deploy |
|---|---|---|---|
| 1 | DigitalOcean | A droplet: Ubuntu 24.04 LTS (x64), **2 GB / 1 vCPU** Basic, region **SFO3 — not Toronto** (the monitor watches Colophon and Durable Runner, which live in TOR1; a shared region means a regional failure lies about all three), your SSH key, monitoring on | Its public IPv4 |
| 2 | DigitalOcean | A cloud firewall on it: inbound **22** from your own address only, **443** from Cloudflare only (`cloudflare-ips.sh --print` prints the ranges); no other inbound | — |
| 3 | DigitalOcean | A block-storage volume for `/opt/smon/data` (10 GB is far beyond the 2 GiB store cap; the point is a store that survives droplet replacement) | Attach on first boot |
| 4 | Cloudflare R2 | One private bucket, `smon-backups`, location hint **WNAM**, **Object Lock enabled with a 7-day default retention** (a ransomware'd droplet can't delete a week of backups) | Your account id |
| 5 | Cloudflare R2 | An API token, **Object Read & Write**, limited to `smon-backups` | Its access key id and secret access key |
| 6 | Cloudflare DNS | `monitor.example.com` → the droplet's IPv4, **proxied** (orange cloud); `status.example.com` → Cloudflare Pages (`outside-check.yml` publishes it) | — |
| 7 | Cloudflare SSL/TLS | Mode **Full (strict)**; an **Origin Certificate** for `monitor.example.com` (RSA, 15 years); **Authenticated Origin Pulls** enabled for the zone (the origin verifies the TLS client is Cloudflare, on top of the firewall) | The certificate and its private key (pasted on the droplet only); Cloudflare's published origin-pull CA cert |
| 8 | Cloudflare | Web Analytics **off** for `monitor.example.com` (it injects a script the CSP blocks) | — |
| 9 | Cloudflare Pages | A `status` Pages project for `status.example.com`, plus an API token with **Pages:Edit** and your account id → these live in **GitHub secrets**, never on the droplet | `CLOUDFLARE_API_TOKEN`, `CLOUDFLARE_ACCOUNT_ID` in the repo's secrets |
| 10 | Email (optional) | An SMTP relay for alert email: `smtps://` credentials (implicit TLS on 465 preferred; STARTTLS 587 where implicit isn't offered) | The DSN, pasted on the droplet only |

What you'll paste on the droplet, and where (DEPLOY.md walks you through each):

- the admin token you generate → `SMON_ADMIN_TOKEN` in `/opt/smon/env` (generated there, never seen elsewhere);
- the origin certificate and key → `/etc/caddy/origin.pem` and `/etc/caddy/origin.key`;
- Cloudflare's origin-pull CA cert → `/etc/caddy/cloudflare-origin-pull-ca.pem`;
- the R2 secret access key → `/opt/smon/credentials/s3_secret_key`;
- the SMTP DSN (if used) → `SMON_SMTP_DSN` in `/opt/smon/env`;
- the R2 account id and access key id (not secret) → `/opt/smon/credentials/r2.env`.

One thing generated on the droplet you **must** copy out: the backup age key's private line
(DEPLOY.md step 9), into a password manager or onto paper. The off-box backups are unreadable
without it.
