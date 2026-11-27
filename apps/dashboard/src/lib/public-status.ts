// What the public status page is allowed to say, decided in one place so it can be
// tested without a browser. The rule throughout: say only what the snapshot supports.

export type PublicService = {
	name: string;
	status: 'up' | 'down' | 'unknown';
	incident_open_since: string | null;
	/** Share of scheduled checks that actually ran, in basis points; null: nothing scheduled yet. */
	coverage_bps: number | null;
	/** Share of the checks that ran which passed, in basis points; null: none ran. */
	success_bps: number | null;
};

export type PublicSnapshot = {
	title?: string;
	generated_at: string;
	expires_at: string;
	monitoring?: 'available' | 'unavailable';
	services: PublicService[];
};

/** `ok`: a fresh snapshot. `stale`: the monitor stopped writing. `absent`: no page here. `unreachable`: no answer. */
export type PageState = 'ok' | 'stale' | 'absent' | 'unreachable';

/**
 * Fresh only when the monitor said it was observing AND the expiry is readable and
 * still ahead. A date that doesn't parse is not evidence that the monitor is alive.
 */
export function isFresh(snap: PublicSnapshot, now: number): boolean {
	return snap.monitoring !== 'unavailable' && Date.parse(snap.expires_at) >= now;
}

export type Verdict = { tone: 'live' | 'attention' | 'draft'; line: string; detail: string };

/** The page's headline. Green needs every listed service to be up on a fresh snapshot. */
export function verdictFor(state: PageState, services: PublicService[]): Verdict {
	if (state === 'unreachable') {
		return {
			tone: 'draft',
			line: 'Status unavailable',
			detail: "The status service didn't answer. This says nothing about the services themselves."
		};
	}
	if (state === 'stale') {
		return {
			tone: 'draft',
			line: 'Monitoring unavailable',
			detail:
				'The monitor has stopped reporting, so current status is unknown. What you see below is the last recorded state.'
		};
	}
	if (services.length === 0) {
		return { tone: 'draft', line: 'Nothing published yet', detail: 'No services are listed on this page.' };
	}
	const down = services.filter((s) => s.status === 'down').length;
	if (down > 0) {
		return {
			tone: 'attention',
			line: down === 1 ? '1 service is down' : `${down} services are down`,
			detail: 'An incident is open. It closes after the service passes its checks again.'
		};
	}
	if (services.some((s) => s.status === 'unknown')) {
		return {
			tone: 'draft',
			line: 'Some services have no recent result',
			detail: 'A service without a recent check is shown as unknown, never as up.'
		};
	}
	return {
		tone: 'live',
		line: 'All listed services are up',
		detail: 'Every service below passed its most recent checks.'
	};
}

/** The badge's tone for one service: a stale page never shows a live colour. */
export function toneFor(state: PageState, status: PublicService['status']): Verdict['tone'] {
	if (state === 'stale') return 'draft';
	return status === 'up' ? 'live' : status === 'down' ? 'attention' : 'draft';
}
