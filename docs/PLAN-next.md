# PLAN — next milestone (M8): response assertions + `HEAD` opt-in

Proposed next slice, derived from `docs/ROADMAP.md` (M0–M7 closed) and `docs/END_GOAL.md`
(the only in-scope contract items still unbuilt). Planning only — no code yet; the third
re-review of `66a31c8..20f0190` is in flight.

## Scope

Per-monitor, optional, bounded **expectations**:

- `method`: `GET` (default) or `HEAD` — the guide's "`GET`/`HEAD` health URLs".
- `expect.statuses`: acceptable status set — ints and inclusive ranges (default `200-299`,
  today's behavior). A status outside the set is `http_fail`, as any non-2xx is today.
- `expect.body_contains`: a plain byte substring, 1–256 bytes, matched literally against
  the bounded body read (≤64 KiB, unchanged). A miss on an acceptable status is failure
  evidence — `http_fail` with `detail: "assert_body"` (a new fixed detail word; guide
  vocabulary + `guide_drift` updated both directions).

Rules pinned at write time (422 `invalid_request`):

- `HEAD` + `expect.body_contains` is refused — a HEAD probe carries no body to assert.
- `expect.statuses`: ≤8 ranges, each `1..=599`, low ≤ high, none overlapping.
- `expect.body_contains`: literal bytes only — never regex (a `.*` stored is a literal
  `.*` searched; no ReDoS class exists because no engine exists).
- Assertions apply to the **final** response of a redirect chain; intermediate 3xx hops
  stay redirect evidence as today.
- Storage: migration `00xx` adds the columns (`method`, `expect_statuses`, `expect_body`)
  — manifest gains only appended lines (the append-only gate watches this).
- The fields never appear in `status.json` (allowlist unchanged) and never in logs or
  error envelopes — committed `detail` stays a fixed vocabulary word.

## Failure cases / attack classes (each into `tests/attacks/mutants/`)

| Test | Mutant it must kill |
|---|---|
| `assertions::a_body_miss_is_failure_evidence` | skip the substring check — a miss passing proves the gate isn't wired |
| `assertions::the_expect_config_is_bounded_at_write` | drop the length/cardinality checks — oversized `body_contains`, `>8` ranges, empty set, `999-100` must all still 422 |
| `assertions::head_and_a_body_expectation_is_refused_at_write` | allow the combo — write must still refuse |
| `assertions::the_body_bound_still_holds` | search the unbounded body — a marker *past* the 64 KiB cap must miss, never pass |
| `assertions::a_literal_regex_is_not_an_engine` | compile `body_contains` as regex — `x.*y` must match only the literal `x.*y` |
| `assertions::expectations_stay_out_of_the_public_snapshot` | write `expect_body`/`method` into `status.json` fields — the allowlist must still exclude them |
| `assertions::patch_revalidates` | skip validation on the edit path — a bad config smuggled through PATCH must still refuse |

Plus the standing classes that already apply unchanged: SSRF (assertions never relax the
target policy), the per-address budget (a `HEAD` probe is still a paid start), tenant
isolation (assertion config is workspace-owned monitor data).

## Load scenario (≥1M requests)

`perf/lab-profile.mjs` shape, assertions enabled on half the registered monitors
(`body_contains` on a fixture that embeds the marker in some origins and drops it in
others — the failure path exercised, not just the pass path): measure per-check CPU and
commit-lag delta vs the no-assertion baseline. Then the droplet soak below.

## Risks

- **Classification choice**: a body miss could be a new `CheckOutcome` variant instead of
  `http_fail`+detail. Detail-on-`http_fail` is smaller (no decode/store migration for
  outcomes) and honest — the status was acceptable; the evidence wasn't. Decide in the
  slice's D-entry before code.
- **`HEAD` semantics**: reqwest must preserve `HEAD` across redirects (no GET rewrite),
  and a HEAD probe must never block reading a body. Pin both in fixture tests.
- **Semantics of "not found"**: a marker past the 64 KiB bound misses — documented as
  "assertion over the observed prefix", never "the target doesn't have it".
- **Vocabulary drift**: `assert_body` joins the detail vocabulary — `guide_drift` both
  directions, dashboard status tokens, and `05-api-cli.md`.
- **Capacity**: unchanged (same slot/per-origin/address bounds) — but the write-time
  capacity message and CLI surface grow fields; keep the envelope identical.

## What END_GOAL claims that this branch has not yet proven

Honest list — evidence, not roadmap:

1. **Response assertions** — not built (`03-coordinator.md`: "assertions later"). This
   slice.
2. **`HEAD` health URLs** — not built ("`GET` only in v1; `HEAD` opt-in later"). This
   slice.
3. **The 24-hour soak under the current standard** — `soak-24h-20261106.log` is no longer
   an empty placeholder (it holds the summarizer's 1 MB `maxBuffer` crash dump), and two
   artifacts exist: the 2026-11-08 run did 576,000 checks / 0 missed / 6,605 incidents /
   13,195 alerts at `a311f90` — **but** it is (a) below the ≥1M standard, (b) salvaged:
   commit-lag and RSS series were in crashed process memory and are lost, and (c) on a
   pre-accounts commit, before this branch's multi-tenant and security work. The
   2026-11-10 run descoped to 1.3 h. A rerun on the merged build at ≥1M checks is owed —
   droplet work.
4. **"Deployed off the target host"** — the install drill is a local fresh-prefix;
   `deploy/droplet/` + `smoke.sh` + the Caddyfile are scripted and unit-pinned, but no
   committed evidence shows the droplet serving the current build. The header-dump
   runbook in `docs/droplet-header-dump.md` is the first artifact of that proof.
5. **"Independently hosted" public status page** — `status.json` + self-expiry +
   `status-check.mjs` are proven; *hosted on an independent host* (off the droplet) is
   designed in the deploy docs but not evidenced.
6. **Multi-tenant soak** — workspaces/seats/plans are this branch's new surface; they
   pass tests but have never been exercised under a real run.
7. **Droplet restore/upgrade drills** — `backup-drill.sh`, `schema-guard.sh`,
   `upgrade.sh`/`rollback.sh` exist and pass local drills; not evidenced on the droplet.
8. **Docs site + changelog** — the workspace rule requires every project's own
   `*-docs` Astro Starlight site with a changelog; none exists for service-monitor.
   Outside END_GOAL, but workspace-owed before "done".
