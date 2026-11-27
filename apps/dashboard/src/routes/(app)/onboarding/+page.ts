import { call } from '#lib/call.js';
import type { PageLoad } from './$types';

export type FirstMonitor = { id: string; name: string; status: 'up' | 'down' | 'unknown' };

// Every tick on the guide is read from the workspace's real state — a monitor that
// exists, a webhook that is saved, a check that was committed — never from a flag the
// page sets itself.
export const load: PageLoad = async ({ fetch }) => {
	const [monitors, webhook] = await Promise.all([
		call<{ monitors: FirstMonitor[] }>('/api/monitors', {}, fetch),
		call<{ webhook: { url: string } | null }>('/api/workspace/webhook', {}, fetch)
	]);
	if (!monitors.ok) {
		return { failure: monitors.failure, monitors: [] as FirstMonitor[], webhookUrl: null, observed: 0 };
	}
	const list = monitors.body.monitors;
	// "First checks land" is about committed evidence, so ask for it: a status can read
	// `up` only after a check, but `unknown` alone can't tell "none yet" from "missed".
	let observed = 0;
	const first = list[0];
	if (first) {
		const stats = await call<{ observed: number }>(`/api/monitors/${first.id}/stats`, {}, fetch);
		if (stats.ok) observed = stats.body.observed;
	}
	return {
		failure: null,
		monitors: list,
		// A viewer can't read the webhook; for them the step simply stays undecided.
		webhookUrl: webhook.ok ? (webhook.body.webhook?.url ?? null) : null,
		observed
	};
};
