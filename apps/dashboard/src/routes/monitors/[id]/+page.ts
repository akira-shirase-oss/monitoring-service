import type { PageLoad } from './$types';
import type { MonitorView } from '../../+page';

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

// Three honest states: the monitor row, "the server cannot see", and "there is no such
// monitor" — a 404 is not a fetch failure and must not be rendered as one.
export const load: PageLoad = async ({ fetch, params }) => {
	try {
		const [monitor, checks, incidents] = await Promise.all([
			fetch(`/api/monitors/${params.id}`),
			fetch(`/api/monitors/${params.id}/checks?limit=50`),
			fetch(`/api/incidents?monitor_id=${params.id}&limit=50`)
		]);
		if (monitor.status === 404) {
			return { state: 'missing' as const };
		}
		if (!monitor.ok || !checks.ok || !incidents.ok) {
			return { state: 'unreachable' as const };
		}
		return {
			state: 'ok' as const,
			monitor: (await monitor.json()) as MonitorView,
			checks: ((await checks.json()) as { checks?: CheckView[] }).checks ?? [],
			incidents: ((await incidents.json()) as { incidents?: IncidentView[] }).incidents ?? []
		};
	} catch {
		return { state: 'unreachable' as const };
	}
};
