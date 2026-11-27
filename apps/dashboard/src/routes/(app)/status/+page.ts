import type { PageLoad } from './$types';

// The public status snapshot — served at /status.json outside /api, so this view needs
// no token and touches no management surface. Three honest states: a fresh snapshot,
// a stale one (dead heartbeat — "monitoring unavailable", rows kept as stale evidence),
// and absent (no snapshot readable — same verdict, no rows at all).
export type ServiceView = {
	alias: string;
	status: 'up' | 'down' | 'unknown';
	incident_open_since: string | null;
};

export type SnapshotView = {
	generated_at: string;
	expires_at: string;
	// `unavailable` when the engine (writer/coordinator) was not observing as it wrote —
	// such a beat is also born expired, so older readers reach the same verdict.
	monitoring?: 'available' | 'unavailable';
	services: ServiceView[];
};

export const load: PageLoad = async ({ fetch }) => {
	try {
		const response = await fetch('/status.json', { cache: 'no-store' });
		if (!response.ok) return { state: 'unavailable' as const };
		const snap = (await response.json()) as SnapshotView;
		const stale = snap.monitoring === 'unavailable' || Date.parse(snap.expires_at) < Date.now();
		return { state: stale ? ('stale' as const) : ('ok' as const), snap };
	} catch {
		return { state: 'unavailable' as const };
	}
};
