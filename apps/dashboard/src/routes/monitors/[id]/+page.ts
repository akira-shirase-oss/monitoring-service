import type { PageLoad } from './$types';
import type { MonitorView } from '../../+page';
import { apiFetch } from '#lib/auth.js';

// Client-side only: the route's id comes from the URL at runtime; the static build's
// 200.html fallback serves the page and the load runs in the browser.
export const prerender = false;

export type CheckView = {
	slot_at: string;
	outcome: string;
	http_status: number | null;
	latency_ms: number | null;
	detail: string | null;
	recorded_at: string;
};

export type IncidentView = {
	id: number;
	monitor_id: string;
	opened_at: string;
	resolved_at: string | null;
	state: 'open' | 'resolved';
};

export type StatsView = {
	window_s: number;
	expected: number;
	observed: number;
	missed: number;
	passed: number;
	coverage_bps: number | null;
	success_bps: number | null;
};

// Four honest states: the monitor row, "the server cannot see", "there is no such
// monitor", and "the server wants a credential" — a 401 is not a fetch failure either.
export const load: PageLoad = async ({ fetch, params }) => {
	try {
		const [monitor, checks, incidents, stats] = await Promise.all([
			apiFetch(`/api/monitors/${params.id}`, undefined, fetch),
			apiFetch(`/api/monitors/${params.id}/checks?limit=50`, undefined, fetch),
			apiFetch(`/api/incidents?monitor_id=${params.id}&limit=50`, undefined, fetch),
			apiFetch(`/api/monitors/${params.id}/stats`, undefined, fetch)
		]);
		if (monitor.status === 401) {
			return { state: 'needsAuth' as const };
		}
		if (monitor.status === 404) {
			return { state: 'missing' as const };
		}
		if (!monitor.ok || !checks.ok || !incidents.ok || !stats.ok) {
			return { state: 'unreachable' as const };
		}
		return {
			state: 'ok' as const,
			monitor: (await monitor.json()) as MonitorView,
			checks: ((await checks.json()) as { checks?: CheckView[] }).checks ?? [],
			incidents: ((await incidents.json()) as { incidents?: IncidentView[] }).incidents ?? [],
			stats: (await stats.json()) as StatsView
		};
	} catch {
		return { state: 'unreachable' as const };
	}
};
