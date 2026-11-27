import type { PageLoad } from './$types';

// The slug comes from the address, so there is nothing to prerender.
export const prerender = false;

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

// A workspace's public status page (no sign-in, no cookie). Four honest states: a
// fresh snapshot; a stale one (the monitor stopped writing: "monitoring unavailable",
// rows kept as last-known evidence); no page at this address; and no answer at all.
export const load: PageLoad = async ({ fetch, params }) => {
	try {
		const response = await fetch(`/s/${encodeURIComponent(params.slug)}/status.json`, {
			cache: 'no-store',
			credentials: 'omit'
		});
		if (response.status === 404) return { state: 'absent' as const, snap: null };
		if (!response.ok) return { state: 'unreachable' as const, snap: null };
		const snap = (await response.json()) as PublicSnapshot;
		// Fresh only when the expiry is readable AND still ahead: a date that doesn't
		// parse is not evidence the monitor is alive.
		const fresh = snap.monitoring !== 'unavailable' && Date.parse(snap.expires_at) >= Date.now();
		const stale = !fresh;
		return { state: stale ? ('stale' as const) : ('ok' as const), snap };
	} catch {
		return { state: 'unreachable' as const, snap: null };
	}
};
