import { call } from '#lib/call.js';
import type { PageLoad } from './$types';

export type IncidentView = {
	id: number;
	monitor_id: string;
	opened_at: string;
	first_fail_at: string | null;
	last_fail_at: string | null;
	resolved_at: string | null;
	state: string;
};

export const load: PageLoad = async ({ fetch }) => {
	const [incidents, monitors] = await Promise.all([
		call<{ incidents: IncidentView[] }>('/api/incidents?limit=200', {}, fetch),
		call<{ monitors: { id: string; name: string }[] }>('/api/monitors', {}, fetch)
	]);
	if (!incidents.ok) {
		return { incidents: [] as IncidentView[], names: {} as Record<string, string>, failure: incidents.failure };
	}
	// A monitor that was deleted since still has its incidents; its name is simply gone.
	const names: Record<string, string> = {};
	if (monitors.ok) for (const m of monitors.body.monitors) names[m.id] = m.name;
	return { incidents: incidents.body.incidents, names, failure: null };
};
