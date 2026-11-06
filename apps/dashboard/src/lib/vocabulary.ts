// The one glossary — plain words for every wire/domain string the UI can show.
// `word` is the literal serialized value (status / outcome / detail / incident fields);
// a Rust drift test pins this set against the real enums so a new word can't ship
// undocumented. `kind` groups the guide; `title`/`body` are what the page renders.

export type GuideWord = {
	word: string;
	kind: 'status' | 'outcome' | 'detail' | 'incident';
	title: string;
	body: string;
};

export const VOCABULARY: GuideWord[] = [
	// --- monitor.status ---
	{
		word: 'up',
		kind: 'status',
		title: 'up',
		body: 'No incident is open and a recent check passed. Green means committed evidence said so — never an assumption.'
	},
	{
		word: 'down',
		kind: 'status',
		title: 'down',
		body: 'An incident is open: consecutive failures crossed the monitor’s open threshold (default 3).'
	},
	{
		word: 'unknown',
		kind: 'status',
		title: 'unknown',
		body: 'No fresh evidence — never observed, or the last slot was missed. It says nothing about the target being up or down.'
	},
	{
		word: 'paused',
		kind: 'status',
		title: 'paused',
		body: 'Scheduling is off for this monitor. Not a verdict about the target — a management state.'
	},

	// --- check.outcome ---
	{
		word: 'pass',
		kind: 'outcome',
		title: 'pass',
		body: 'The target answered with an acceptable 2xx status inside the deadline.'
	},
	{
		word: 'http_fail',
		kind: 'outcome',
		title: 'HTTP failure',
		body: 'The target answered, but with a non-2xx status (or a redirect chain that never resolved). Evidence about the target.'
	},
	{
		word: 'transport',
		kind: 'outcome',
		title: 'transport failure',
		body: 'No usable response: DNS, connect, TLS/certificate or body-read failure. The detail column says which.'
	},
	{
		word: 'timeout',
		kind: 'outcome',
		title: 'timeout',
		body: 'The probe exceeded its total deadline — evidence about this observation, not proof the target never answered.'
	},
	{
		word: 'missed',
		kind: 'outcome',
		title: 'missed slot',
		body: 'The scheduled slot never ran — an overloaded window or a paused span. No evidence at all: resets streaks, counts as neither pass nor fail.'
	},

	// --- check.detail (transport taxonomy) ---
	{ word: 'connect', kind: 'detail', title: 'connect', body: 'Couldn’t open a connection — refused, unreachable, or reset.' },
	{ word: 'tls', kind: 'detail', title: 'TLS', body: 'TLS handshake failed for a reason that isn’t a certificate classification below.' },
	{ word: 'cert_expired', kind: 'detail', title: 'certificate expired', body: 'The certificate’s notAfter date has passed — the most common TLS alert.' },
	{ word: 'cert_not_yet_valid', kind: 'detail', title: 'certificate not yet valid', body: 'The certificate’s notBefore is in the future — clock skew or a rotated-too-early cert.' },
	{ word: 'cert_name_mismatch', kind: 'detail', title: 'certificate name mismatch', body: 'The certificate is trusted but for a different hostname (SAN doesn’t match).' },
	{ word: 'cert_untrusted', kind: 'detail', title: 'certificate untrusted', body: 'The chain doesn’t anchor to a public root — self-signed or an internal CA not added via extra_roots.' },
	{ word: 'cert_revoked', kind: 'detail', title: 'certificate revoked', body: 'The certificate has been revoked.' },
	{ word: 'rejected_local', kind: 'detail', title: 'rejected (local address)', body: 'The target resolved to a local/private address — refused by the SSRF policy before dialing.' },
	{ word: 'body', kind: 'detail', title: 'body read', body: 'Connected and got a response, but reading the body failed (incl. over the 64 KiB cap).' },
	{ word: 'request', kind: 'detail', title: 'request', body: 'The request itself could not be built or sent.' },

	// --- incident fields/state ---
	{
		word: 'first_fail_at',
		kind: 'incident',
		title: 'first failure',
		body: 'When the failing streak actually began — derived from committed check rows, so it stays right across restarts.'
	},
	{
		word: 'opened_at',
		kind: 'incident',
		title: 'opened',
		body: 'When the open threshold was crossed and the incident (and alert) fired. Notice time ≠ failure-start time.'
	},
	{
		word: 'last_fail_at',
		kind: 'incident',
		title: 'last failure',
		body: 'The most recent failing slot while the incident stays open — how long it’s been failing.'
	},
	{
		word: 'resolved_at',
		kind: 'incident',
		title: 'resolved',
		body: 'When the recovery threshold (consecutive passes, default 2) was satisfied.'
	},
	{
		word: 'open',
		kind: 'incident',
		title: 'open incident',
		body: 'The incident is active: the target is failing and the streak has not recovered.'
	},
	{
		word: 'resolved',
		kind: 'incident',
		title: 'resolved incident',
		body: 'The incident is closed — enough consecutive passes committed to satisfy the recovery threshold.'
	}
];

// Concepts the guide explains that aren't literal wire strings.
export type GuideConcept = { title: string; body: string };
export const CONCEPTS: GuideConcept[] = [
	{
		title: 'coverage vs success',
		body: 'Coverage: of the slots that should have run, how many produced any observation. Success: of the observed slots, how many passed. Coverage can be 100% while success is 0% — they are separate claims and the UI keeps them separate.'
	},
	{
		title: 'generation',
		body: 'The monitor’s config version — bumps once on every write (schedule, thresholds, pause). A probe started on an older generation is dropped on commit: stale results can never land.'
	},
	{
		title: '“cannot see” / server unreachable',
		body: 'The monitor server itself isn’t answering. This says nothing about your targets — it means the monitor can’t see right now.'
	},
	{
		title: '“monitoring unavailable”',
		body: 'The status snapshot heartbeat is stale or missing — the monitor may be down. Read stale rows as last-known evidence, not current state.'
	},
	{
		title: 'the strip’s empty cells',
		body: 'In the uptime strip, a dashed cell marks slots where no observation exists — a hole in evidence is drawn as a hole, never interpolated green.'
	},
	{
		title: 'thresholds (fails to open / passes to recover)',
		body: 'Per-monitor incident sensitivity: how many consecutive failures open an incident (default 3), how many consecutive passes close it (default 2). Both editable per monitor, 1–10.'
	}
];
