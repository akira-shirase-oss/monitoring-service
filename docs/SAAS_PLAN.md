# Service Monitor as a SaaS — plan

Status: **plan, not built.** Written 2026-11-13. Nothing here is deployed yet; numbers measured on the droplet do
not exist yet and are marked *unmeasured*.

## The goal

Service Monitor runs as its own hosted product: a person signs up, gets a workspace, adds the URLs they care
about, connects an alert channel, and gets a public status page — honest `unknown` and coverage included. The first
users are the maintainer's friends; payments are bypassed.

Decisions already made (2026-11-13, by the maintainer):

- **Independent product** with its own repository, accounts, login, dashboard and droplet. It meets Colophon and
  Durable Runner only through public APIs and tokens.
- **Its own 2 GB DigitalOcean droplet, separate from the others**, so it observes them from outside.
- **Neon Postgres** for account records; see "Data" for what stays local.
- **Landing page content is edited in Colophon** and served as a static site.
- **Payments are bypassed:** a free plan with limits.

## Where Service Monitor is today

- One operator: `SMON_ADMIN_TOKEN` bearer for CLI/scripts, or the `smon_session` cookie from `POST /api/login`.
  There are no user accounts or workspaces.
- API: `/monitors`, `/monitors/{id}/checks`, `/monitors/{id}/stats`, `/incidents`, `/maintenance/{id}`, `/backup`,
  `/livez`, `/readyz`, `/version`.
- Data: SQLite (`SMON_DATABASE_URL`), with retention, size cap (`SMON_MAX_DB_BYTES`) and `VACUUM INTO` backups.
- Alerts: one global webhook (`SMON_WEBHOOK_URL`, HMAC-signed) and one SMTP destination (`SMON_SMTP_*`).
- Dashboard: a small SvelteKit app (`apps/dashboard`: overview, monitor detail, status, guide) and a static status
  page (`apps/status`).
- Probe targets are SSRF-guarded: the guard refuses private, loopback and link-local targets, validates DNS
  answers and revalidates every redirect hop. The alert webhook URL is **not** (D-017): today only the operator sets
  it, which is not a privilege boundary; once tenants set webhook URLs it is.
- Deploy: `deploy/monitor-server.service` (systemd), launchd plist, `deploy/RUNBOOK.md`.
- Readiness: feature-complete (M0–M7); the 24-hour soak is still owed.

## The gaps between "my monitor" and "SaaS"

| Gap | What has to change |
|---|---|
| Accounts | Signup (invite-code gated for the beta), email verification, sign-in, password reset, sessions per user. |
| Workspaces | Every monitor, check, incident, maintenance window, alert channel and status page belongs to a workspace; every query is scoped by it. Members and roles (owner, member, viewer). |
| Tokens | Per-workspace API tokens replace the single admin token for scripts and integrations (Colophon uses one). |
| Alert channels | Per-workspace webhooks and email recipients, with a "send test alert" button, replacing the global env channels. **P0 gate (D-017):** a tenant's webhook URL goes through `ProbePolicy` and the validating resolver when saved and at every send, no redirects — otherwise a tenant points alerts at loopback, the VPC or the metadata address. |
| Status pages | One public status page per workspace, at its own path or subdomain. |
| Limits | Free plan: number of monitors, minimum interval (e.g. 60 s), alert channels, retention days. Per-workspace caps on top of the existing global and per-origin concurrency, so one tenant cannot crowd the probe workers or use the service to hammer a third-party site. |
| Dashboard | Rebuilt on a copy of Colophon's `@colophon/ui` (Tailwind v4 + shadcn-svelte), which this repo then owns: shell, auth forms, onboarding, settings (members, tokens, alert channels), monitors, incidents, maintenance, status page editor. Own accent and logo. |
| Onboarding | Sign up → create workspace → add first URL → connect an alert channel → see the first checks land. |
| Account lifecycle | Delete workspace (and its evidence), export evidence. |

## Data

The probe loop writes evidence on every slot; keep that write path local.

- **SQLite on the droplet** keeps check evidence, incidents and the alert outbox (today's engine, now with a
  `workspace_id` on every row).
- **Neon** holds accounts, workspaces, members, tokens and plan limits — the records that change rarely.
- The engine must keep observing when Neon is unreachable: it reads its monitor configuration from SQLite, which
  is synchronised from the account side, and authentication fails closed while monitors keep running.

Alternative to decide on: keep everything in SQLite and skip Neon for this product. It is simpler and keeps the
product fully self-contained; the cost is no managed database for account records.

## Droplet layout (one 2 GB droplet, separate)

```
Caddy (TLS)  →  monitor-server (API + dashboard + probe workers + alert deliverer)
                 ├── SQLite on a DigitalOcean volume (evidence)
                 └── Neon Postgres (accounts)
```

Memory and probe capacity are *unmeasured* on the droplet; the plan limits come from those measurements.

## Monitoring the other products

The maintainer's own workspace monitors Colophon (`/readyz`, the Studio, the landing pages) and Durable Runner
(`/health`) through their public URLs — exactly as a customer would. If a Colophon user connects a Service
Monitor token, Colophon registers its sites in that user's workspace through the public API.

## Phases and exit gates

| Phase | Outcome | Exit gate (evidence) |
|---|---|---|
| S0 Deploy as-is | Today's single-operator monitor on its own droplet, watching Colophon and Durable Runner. | Coverage and alert delivery recorded over a week; idle memory recorded. |
| S1 Accounts + workspaces | Signup, workspaces, members, per-workspace tokens; every query scoped. | Isolation tests: workspace A can never read, edit or be alerted about workspace B's monitors through API, dashboard or status page. |
| S2 Dashboard + onboarding | The rebuilt dashboard on the copied UI, onboarding journey, alert channel test send, status page editor. | Playwright journey: signup → first monitor → test alert → public status page. |
| S3 Limits | Plan limits and per-workspace probe caps. | Noisy-tenant run: one workspace at its limit does not move another workspace's slot coverage. |
| S4 Load on the droplet | ≥1M checks across many workspaces, plus the API under load, generated from another machine. | Results committed under `perf/results/` with droplet spec and commit hash. |
| S5 Friends beta | Invite codes go out. | The 24-hour soak on the droplet; runbook exercised (restore, workspace deletion). |

## Open questions

1. ~~Accounts in Neon, or keep the whole product on SQLite?~~ Decided (D-020): everything in the same SQLite store; no Neon.
2. ~~Status pages on a path (`/s/<workspace>`) or on subdomains?~~ Decided (D-021): `/s/<workspace-slug>` on the same origin; subdomains only if asked later.
3. Email provider for alerts and verification? Stays lettre SMTP; the provider is a deploy-time choice, not a code decision.
4. ~~Free-plan limits: monitors, minimum interval, retention?~~ Decided (D-022):
   `workspaces.plan` tiers (`free`/`operator`) — provisionally 10 monitors, a 60 s
   interval floor and shorter evidence horizons on `free`; the operator tier
   defers to the configured ceilings. All provisional until droplet measurements.
5. ~~S4's ≥1M checks: one server dispatches at most 20 probes per 1 s tick.~~ Decided (D-018): phases are spread per monitor and the global budget follows the schedule up to 512 slots, with a named refusal past it. One server now serves up to ~1,900 monitors at 15 s (~128 checks/s, ≈2.2 h for 1M checks); the long run itself is still to do.
