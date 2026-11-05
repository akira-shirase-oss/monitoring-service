# End goal

What "done" means for the Service Monitor rebuild — derived from
`Project_Guides/02_Service_Monitor_Rebuild.docx` (the contract) and the decisions in
`docs/DECISIONS.md`. When this file and the guide disagree, the guide wins.

## The product

A self-hosted availability monitor for a small number of authorized HTTP services. It tells the
truth about availability — including when the monitor itself cannot see — for its first users:
the maintainer and developers operating their own services.

**Done means:** another developer can install it, register a monitor, deliberately break the
target, receive one logical incident event and one recovery event, and correctly distinguish a
target outage from missing monitoring data — with the monitor deployed off the target host.

## In scope (the full direction)

- Register, edit, pause and delete monitors (`GET`/`HEAD` health URLs) with safe response
  assertions.
- Scheduled probing: one attempt per slot, bounded everything, no overlap, no catch-up storm.
- Incident lifecycle: three consecutive failures open, two fresh passes recover, `unknown` for
  missing coverage, open incidents preserved across restarts.
- Durable alert delivery (webhooks first, email later) with finite retries and stable event ids.
- The dashboard (management + investigation) and an independently hosted, sanitized, expiring
  public status page.
- An independent heartbeat that makes "the monitor is down" visible as such.
- Maintenance windows, check/incident history with retention bounds, and a CLI for everything.
- Restore/upgrade drills and honest documentation.

## Out of scope (explicit exclusions from the guide)

- APM, tracing, log aggregation, AI diagnosis, business-job execution — this is not Datadog.
- Probing arbitrary targets without authorization; POST/write checks against third parties.
- Multi-region consensus, HA clustering, a SaaS offering.
- Claimed benchmarks: all numbers in docs are proposed lab workloads until a run produces
  evidence.

## Ship gate (from the guide)

Ship when another developer can install it, obtain a useful alert, and distinguish a target
outage from missing monitoring data.
