import type { PageLoad } from './$types';

export type MonitorView = {
	id: string;
	name: string;
	url: string;
	interval_s: number;
	timeout_s: number;
	generation: number;
	paused: boolean;
	status: 'up' | 'down' | 'unknown';
};

// The dashboard must tell the truth about the monitor itself: an unreachable server is shown
// as "cannot see", never as an empty list that reads like "everything is fine".
export const load: PageLoad = async ({ fetch }) => {
	try {
		const response = await fetch('/api/monitors');
		if (!response.ok) {
			return { monitors: [] as MonitorView[], reachable: false as const };
		}
		const body = (await response.json()) as { monitors?: MonitorView[] };
		return { monitors: body.monitors ?? [], reachable: true as const };
	} catch {
		return { monitors: [] as MonitorView[], reachable: false as const };
	}
};
