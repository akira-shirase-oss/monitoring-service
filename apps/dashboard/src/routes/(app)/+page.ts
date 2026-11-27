import type { PageLoad } from './$types';

// The engine verdict the API serves beside the list — the same one `/readyz` reports.
// `unavailable`: not observing (writer/coordinator down or stalled, store writes failing);
// every status then reads `unknown`. `degraded`: observing, but alerts, the public
// heartbeat or retention have stopped.
export type Monitoring = { status: 'ok' | 'degraded' | 'unavailable'; faults: string[] };

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
	/** `GET` or `HEAD`. */
	method: string;
	/** The passing statuses as stored: `200-299`, or a comma-separated list of codes and ranges. */
	expect_statuses: string;
	expect_body: string | null;
};

import { apiFetch } from '#lib/auth.js';

// The dashboard must tell the truth about the monitor itself: an unreachable server is shown
// as "cannot see", a 401 as "needs a token" — never as an empty list that reads like
// "everything is fine".
export const load: PageLoad = async ({ fetch }) => {
	try {
		const response = await apiFetch('/api/monitors', undefined, fetch);
		if (response.status === 401) {
			return { monitors: [] as MonitorView[], monitoring: null, reachable: true as const, needsAuth: true as const };
		}
		if (!response.ok) {
			return { monitors: [] as MonitorView[], monitoring: null, reachable: false as const, needsAuth: false as const };
		}
		const body = (await response.json()) as { monitors?: MonitorView[]; monitoring?: Monitoring };
		return {
			monitors: body.monitors ?? [],
			monitoring: body.monitoring ?? null,
			reachable: true as const,
			needsAuth: false as const
		};
	} catch {
		return { monitors: [] as MonitorView[], monitoring: null, reachable: false as const, needsAuth: false as const };
	}
};
