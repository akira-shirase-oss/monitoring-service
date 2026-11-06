import type { PageLoad } from './$types';

export type MonitorView = {
	id: string;
	name: string;
	url: string;
	interval_s: number;
	timeout_s: number;
	failures_to_open: number;
	passes_to_recover: number;
	generation: number;
	paused: boolean;
	status: 'up' | 'down' | 'unknown';
};

import { apiFetch } from '#lib/auth.js';

// The dashboard must tell the truth about the monitor itself: an unreachable server is shown
// as "cannot see", a 401 as "needs a token" — never as an empty list that reads like
// "everything is fine".
export const load: PageLoad = async ({ fetch }) => {
	try {
		const response = await apiFetch('/api/monitors', undefined, fetch);
		if (response.status === 401) {
			return { monitors: [] as MonitorView[], reachable: true as const, needsAuth: true as const };
		}
		if (!response.ok) {
			return { monitors: [] as MonitorView[], reachable: false as const, needsAuth: false as const };
		}
		const body = (await response.json()) as { monitors?: MonitorView[] };
		return { monitors: body.monitors ?? [], reachable: true as const, needsAuth: false as const };
	} catch {
		return { monitors: [] as MonitorView[], reachable: false as const, needsAuth: false as const };
	}
};
