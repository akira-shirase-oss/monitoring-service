import { isFresh, type PublicSnapshot } from '#lib/public-status.js';
import type { PageLoad } from './$types';

// The slug comes from the address, so there is nothing to prerender.
export const prerender = false;

export type { PublicService, PublicSnapshot } from '#lib/public-status.js';

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
		const stale = !isFresh(snap, Date.now());
		return { state: stale ? ('stale' as const) : ('ok' as const), snap };
	} catch {
		return { state: 'unreachable' as const, snap: null };
	}
};
