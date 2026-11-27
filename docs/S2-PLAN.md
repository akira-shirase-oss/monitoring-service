# S2 — Dashboard + onboarding: the plan

SAAS_PLAN's S2 row: *the rebuilt dashboard on the copied UI, the onboarding journey, the
alert-channel test send, the status page editor — Playwright journey: signup → first
monitor → test alert → public status page.* This document is the screen list, the carry-over
inventory, the exact `@colophon/ui` copy list, the backend gaps, and the slice order with
one Playwright journey each. No code in this document; it is the contract the slices build to.

## Design contract — SUPERSEDED (maintainer correction, 2026-11-19)

**Binding correction:** Service Monitor must NOT use Colophon's look — not its palette, its
Geist Pixel + Inter pairing, the inverted panel, the pixel motifs, or its surfaces. The
instruction below to draw "in the maintainer's taste" was Colophon's taste and is withdrawn.
Smon's own design system arrives later as maintainer-chosen screenshot options — after
Colophon is done and deployed — and drops in by replacing ONE file,
`src/lib/components/ui/styles.css`: every component consumes its token names, so no
component changes when it lands. Until then that file holds deliberate neutral placeholder
tokens (zero-chroma neutrals, one generic blue, platform font stacks — no webfonts at all).

What stays from this plan: the copied shadcn-svelte component code and structure, the
routes, the states, the data-testids and the Playwright journeys — the skeleton, not the
skin. The historical contract follows for the record only.

### Superseded contract (kept for the record)

- **Type:** Geist Pixel for page titles and big figures; Inter everywhere else; JetBrains
  Mono for data — URLs, event ids, secrets, timestamps.
- **One inverted panel** per flow — white on dark, black on light — reserved for the
  onboarding hero. Nothing else inverts.
- **Section cards sit on a ground darker than the page** (the `--card` vs `--background`
  step in the token file already encodes this).
- **One primary blue action per screen** — Service Monitor's own accent (a signal blue at
  `--brand`/`--primary`); every other action is an outline button.
- **Pixel-icon empty states with one Geist Pixel line** — never a paragraph of apology.
- **A LIGHT sidebar with a rounded, inset active pill** — shadcn `sidebar` variant `inset`,
  the pattern `apps/studio/.../[project]/+layout.svelte` already uses.
- **One continuous surface** — no frame between sidebar and content; the only divider is a
  top-bar hairline.
- **16px nav rows**, grouped and labelled.
- **Motion:** no scroll reveals; one-time and on-interaction motion only (the kit's
  `page-enter`, `unfold`, `pop`, `breathe`, `lift` set is already exactly this).
- **Work screens stay calm and dense** — no hero banners on work screens, no marketing
  cards, no decoration that isn't evidence.
- **Status speaks in word + mark, never colour alone** (the token file's own rule; smon's
  `VOCABULARY` carries the words): up → `--live`, down → `--attention`, pending/flapping →
  `--pending`, unknown/paused → `--draft`.

### Font sourcing note

The copied `styles.css` imports DM Sans as `--font-display`; the taste is Geist Pixel.
Keep the token name `--font-display` and swap the import: `@fontsource/geist-pixel` if
published by then, else the `geist` npm package / a vendored woff2 — checked at build time
(S2a). The swap is one line; no component names a font family.

## What to copy from `@colophon/ui` (colophon-cms `main`, `packages/ui/src/lib/`)

The repo's existing copy lives at `apps/dashboard/src/lib/components/ui/` (badge, button,
card today). Extend that directory — this repo then owns the copy, per the workspace rule.
`app.css` imports the copied `styles.css` and keeps `@source` pointing at it.

### Non-component files

| Copy | Why |
|---|---|
| `styles.css` | The whole token system (paper/ink ramp, shadows, sidebar tokens, status hues). Re-skin: `--brand`/`--primary` to smon's signal blue, `--font-display` to Geist Pixel, status hues re-mapped to the monitor vocabulary. Everything else stays. |
| `utils.ts` | `cn()` — every copied component uses it. |
| `hooks/is-mobile.svelte.ts` | `sidebar` reads it for the mobile sheet. |

### Component directories to copy

| Directory | Used by |
|---|---|
| `sidebar` (+ `scroll-area`, `separator`, `sheet` for the mobile collapse) | The app shell — light sidebar, inset pill, workspace switcher at the top, account menu at the foot. |
| `dropdown-menu` | Workspace switcher, account menu, row actions. |
| `button`, `button-group` | Everywhere; `button-group` for the test-alert/save row. |
| `card` (all parts incl. `card-action`, which today's copy lacks) | Section cards on the darker ground. |
| `badge` | Status pills — extend its variants: `up`/`down`/`unknown`/`paused` exist today, add `pending`. |
| `input`, `input-group`, `label`, `field` | Every form — `field` gives label+control+error in one row. |
| `native-select`, `select` | Role picker (native-select), interval/timeout pickers. |
| `switch`, `checkbox` | Monitor pause, status-page monitor visibility. |
| `radio-group` | Role choice where a select reads worse (token creation). |
| `table` | Members, tokens, incidents, check history. |
| `tabs` | Monitor detail sections (Checks / Maintenance / Settings) and settings sub-nav. |
| `dialog`, `alert-dialog` | Token secret shown once (dialog), destructive confirms — delete monitor, revoke token, remove member (alert-dialog). |
| `alert` | Banners: "cannot see", engine degraded, refused_target errors. |
| `sonner` | Toasts for saved/copied/test-send-ok (dep: `svelte-sonner`). |
| `empty` | Every empty state — pixel-icon media + one Geist Pixel line + one action. |
| `skeleton`, `spinner` | Loading rows; the first-checks-land wait. |
| `tooltip` | Nav tooltips when collapsed, status hints, coverage figures. |
| `collapsible` | TechnicalDetails — "operator token" sign-in path, raw payload details. |
| `avatar` | Member rows (initials), account menu. |
| `progress` | The coverage-vs-success figure bars on monitor detail. |
| `kbd` | Invite code / token display — cheap, keeps the mono look consistent. |

### Deliberately left out

`command`/`navigation-menu`/`menubar` (no command palette in S2), `form` + `formsnap` +
`sveltekit-superforms` (smon forms post JSON to the API by hand — no superforms dep),
`calendar`/`range-calendar` (maintenance windows use datetime-local inputs first; revisit if
that reads badly), `chart` + `layerchart` (coverage-vs-success is two figures + the check
strip; a sparkline is a later polish item), `carousel`, `resizable`/`paneforge`, `toggle*`,
`slider`, `pagination` (lists are bounded and short), `context-menu`, `hover-card`,
`input-otp`, `breadcrumb`, `accordion`, `aspect-ratio`, `attachment`, `bubble`, `code-block`,
`drawer`, `item`, `marker`, `message*`, `questionnaire`, `toolbar`, `trace`.

### From the Studio app (port the pattern, not a package copy)

`apps/studio/src/lib/shell/` is where the taste actually lives:

| Port | Becomes |
|---|---|
| `Frame.svelte` | `AuthShell` — the mark + top bar around login/signup/onboarding (the only pages outside the sidebar shell). |
| `AuthForm.svelte` | Sign-in/sign-up forms — field rows, error verbatim, busy state. |
| `EmptyState.svelte` | Wraps `ui/empty` + pixel icon + one Geist Pixel line. |
| `FormRow.svelte` | Field rows inside settings cards. |
| `PageHeader.svelte` | Page title (Geist Pixel) + lede + action slot. |
| `Problem.svelte` | The API error envelope rendered verbatim — the `readEnvelope` convention in pixels. |
| `StatusBadge.svelte` | `MonitorBadge` — word + mark mapped through `VOCABULARY`, never colour-only. |
| `AccountMenu.svelte` | Sidebar-foot account menu: email, workspace role, sign out. |
| `ThemeToggle.svelte` + `theme.ts` | Dark/light with the circle-clip transition (already one-time motion). |
| `Mark.svelte` | The smon mark — own logo. |
| `TechnicalDetails.svelte` | Collapsible disclosure for the operator-token sign-in and raw error detail. |
| `FirstRun.svelte` + `firstRun.ts` | The onboarding checklist machinery — every item derived from real state, never ticked by hand, role-aware, dismissible. |
| `CommandPalette.svelte` | **Defer** — not in the S2 screen list. |

### What carries over from the current dashboard

- `CheckStrip.svelte` + `check-strip.ts` — the committed-evidence strip; restyle onto
  tokens, keep the semantics exactly (missed slots render missed, never interpolated green).
- `MonitorForm.svelte` + `monitor-form.ts` — the create/edit form; rebuilt on
  `field`/`input`/`native-select`.
- `vocabulary.ts` — the drift-pinned wire-word glossary; unchanged.
- `api.ts` (`readEnvelope`) — server's `error.message` shown verbatim; extended with
  `X-Smon-Workspace` on `apiFetch` when a non-home workspace is picked.
- `auth.ts` — extended: `login(email, password)` and the existing token path; `logout`;
  the cookie story is unchanged.
- `routes/status/+page.svelte` — the in-app snapshot view; restyled, kept.
- `tests/dashboard.spec.ts` + `screenshots.spec.ts` — the journey and the UI-audit
  harness; every existing journey is re-pointed at the new auth and kept passing.

## Backend gaps S2 needs (API additions — flag for the coordinator)

The UI below assumes these small endpoints; each is named so the backend slice can land
inside the same commit as its screen or ahead of it:

1. **`PATCH /api/workspace`** — rename the workspace (`{name}`; slug stays). Onboarding
   step 1 names the home workspace created at signup. Member work.
2. **`GET /api/workspaces`** — the caller's memberships `{workspace_id, name, slug, role}`.
   The switcher can't ask for `X-Smon-Workspace` values it doesn't know; today nothing
   lists them. Any role.
3. **`POST /api/workspace/webhook/test`** — "send test alert": an *inline* guarded send
   through the same `ProbePolicy` + `PolicyResolver` + `attempt_deadline` (never an outbox
   row — no incident to reference) answering `{ok, detail}` in the classified taxonomy
   (`delivered`, `rejected_local`, `http_500`, `deadline`, …). Member work.
4. **`GET /api/workspace/status-page` + `PUT`** — `{slug, title, public_monitor_ids[]}`
   (columns new — S2's first migration), and the public **`GET /s/{slug}`** (+ `GET
   /s/{slug}/status.json`) serving a workspace-scoped snapshot with the same staleness
   honesty as `/status.json`. The biggest backend piece; S2f's dependency.
5. **`GET /api/maintenance`** — workspace-wide window list. Optional: the page can iterate
   `GET /api/monitors/{id}/maintenance` over the (bounded, small) monitor list client-side;
   decide at build time.

`apiFetch` gains `X-Smon-Workspace` from the picked workspace (a layout-level store);
absent the header the server serves the account's home workspace — correct default.

## Screen list

Routes inside the sidebar shell unless noted. **Empty** = the pixel-icon + Geist Pixel line.
**Errors** = `Problem` rendering the server's `error.message` verbatim; 401 → sign-in;
403 → the role's read-only rendering, not a hidden page.

| Route | Content | Primary action | Empty | Notes |
|---|---|---|---|---|
| `/login` (auth shell) | Email + password; collapsible "sign in with an operator token" | Sign in (primary) | — | 401 verbatim; 429/503 show Retry-After text. |
| `/signup` (auth shell) | Email, password, invite code | Create account (primary) | — | Invite failures are a uniform 403 by design — show it, don't decorate it. |
| `/` — Monitors | Rows: name, `MonitorBadge`, `CheckStrip`, coverage % + success %, next-slot hint | New monitor (primary, member+) | "Nothing watched yet" → onboarding | "Cannot see" banner on API outage; engine-degraded banner from readyz. |
| `/monitors/[id]` | Status + two figures — **coverage vs success** ("looked at N of M slots" / "passed M of those"), checks table (outcome + detail words), maintenance windows sub-section, settings tab | Edit (member+) | "No checks committed yet" | Detail words through `VOCABULARY`; paused = management state, not a verdict. |
| `/incidents` | Open then recently-closed: monitor, opened_at, recovered_at, duration, delivery `last_error` where shown | — | "No incidents recorded" | Honest wording — never "all healthy". |
| `/maintenance` | Workspace-wide upcoming + active windows across monitors | New window (member+) | "No windows scheduled" | Aggregate client-side unless gap #5 lands. |
| `/onboarding` | **Inverted-panel hero** + derived checklist: name workspace → first URL → alert channel → first checks land | The next undone step | — | `FirstRun` machinery: state-derived ticks, dismissible, role-aware. |
| `/settings/members` | Roster table (email, role, added); add by email + role; role select; remove | Add member (primary, owner) | — (roster always ≥1) | Viewer sees the roster read-only (the API allows it); last-owner refusal = server message. |
| `/settings/tokens` | Table (name, role, `last_used_at`, created); create → secret **shown once** in a dialog with copy; revoke | New token (owner) | "No API tokens" | Mint is viewer\|member only — server refuses owner twice; show that verbatim if tried. |
| `/settings/alerts` | Webhook card: current URL + secret (copy), save URL, remove, **send test alert** → result inline | Save webhook (primary); Send test alert (outline) | "No alert channel connected" | Save refusal shows `refused_target`/`invalid_request` verbatim; test result in the classified taxonomy. |
| `/settings/status-page` | Slug, title, monitor-visibility checkboxes, preview link → `/s/<slug>` | Save (primary) | — | Depends on gap #4. |
| `/s/{slug}` (public, no auth, minimal frame) | Per-workspace status page: title, chosen monitors' honest status + coverage, snapshot timestamp | — | "Nothing public yet" | Stale/missing snapshot → "monitoring unavailable", never a green board — the dashboard's own honesty rule, public. |
| `/status` | The operator's global snapshot (current page, restyled) | — | — | Keep — it is the internal view of the same snapshot. |
| `/guide` | The `VOCABULARY` glossary | — | — | Carry over restyled; the drift test keeps the set honest. |

## Slices, in order

Each lands green through `heavy.sh` (svelte-check + dashboard build + Playwright) and keeps
every earlier journey passing.

- **S2a — Kit copy, shell, auth.**
  Copy the listed files/components into `apps/dashboard/src/lib/components/ui/`, re-skin
  `styles.css` (signal-blue accent, `--font-display` → Geist Pixel, status remap), port the
  shell pieces (`AuthShell`, `AuthForm`, `EmptyState`, `Problem`, `PageHeader`,
  `MonitorBadge`, `AccountMenu`, `ThemeToggle`, `Mark`, `TechnicalDetails`), build the
  sidebar layout with the workspace switcher slot, `/login` (email+password, token
  collapsible), `/signup` (invite field), `/logout` wiring.
  **Journey:** `signup with a minted invite → lands in the empty app → sign out → sign
  back in → sidebar shows the workspace`.

- **S2b — Monitors + detail on the new shell.**
  Port `/` and `/monitors/[id]`; coverage and success as two separate figures from
  `/monitors/{id}/stats`; checks table through `VOCABULARY`; maintenance sub-section.
  **Journeys (ported):** `register → probes land → badge up`; `three failures → incident →
  detail tells the truth`; `paused window → missing slots, never interpolated`; `API
  outage → cannot-see banner`; plus new: `coverage and success shown as separate numbers`.

- **S2c — Onboarding.**
  The inverted-panel hero + `FirstRun`-derived checklist — needs gaps #1 (rename) and #3
  (test send) landed.
  **Journey:** `fresh signup → name the workspace → add first URL → save webhook → send
  test alert → stub receiver gets the signed POST → first checks land → checklist
  completes and can be dismissed`.

- **S2d — Incidents + maintenance pages.**
  `/incidents` (open + recently closed), `/maintenance` (workspace-wide, client-aggregated
  unless gap #5 lands).
  **Journey:** `incident opens → listed with monitor + duration → recovers → moves to
  closed`; `window added → listed → cancelled`.

- **S2e — Settings: members, tokens, alerts.**
  `/settings/members` (roster + role admin), `/settings/tokens` (shown-once dialog,
  revoke), `/settings/alerts` (webhook card + test alert).
  **Journeys:** `owner adds a viewer → the viewer's session reads the roster and every
  write is refused`; `mint member token → secret shown once → the token calls the API →
  revoked → 401`; `save webhook → send test alert → the classified result shows, and a
  refused_target URL never saves`.

- **S2f — Status page editor + the public page.**
  Gap #4 lands first (the only real backend slice). `/settings/status-page` + `GET /s/{slug}`.
  **Journey:** `editor sets slug, title and visible monitors → a logged-out browser at
  /s/<slug> sees only the chosen monitors with honest states → snapshot goes stale →
  "monitoring unavailable"`.

## Out of scope for S2

Plan limits (S3), billing, a second alert channel (SMTP is operator config), the command
palette, sparkline charts, localization, the docs site and changelog (workspace rule —
its own stream), and the landing page (its own repo).
