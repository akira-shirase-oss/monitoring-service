import { call } from '#lib/call.js';
import type { PageLoad } from './$types';

export type WindowView = {
	id: number;
	monitor_id: string;
	starts_at: string;
	ends_at: string;
	reason: string | null;
	active: boolean;
	created_at: string;
};
export type MonitorName = { id: string; name: string };

// There is no workspace-wide window list in the API: read each monitor's own. The
// monitor count is bounded by the plan, and a monitor whose read fails is named rather
// than silently dropped — a missing row must never read as "nothing scheduled".
export const load: PageLoad = async ({ fetch }) => {
	const monitors = await call<{ monitors: MonitorName[] }>('/api/monitors', {}, fetch);
	if (!monitors.ok) {
		return { monitors: [] as MonitorName[], windows: [] as WindowView[], unread: [] as string[], failure: monitors.failure };
	}
	const list = monitors.body.monitors;
	const answers = await Promise.all(
		list.map((m) => call<{ windows: WindowView[] }>(`/api/monitors/${m.id}/maintenance`, {}, fetch))
	);
	const windows: WindowView[] = [];
	const unread: string[] = [];
	answers.forEach((answer, i) => {
		const monitor = list[i];
		if (monitor === undefined) return;
		if (answer.ok) windows.push(...answer.body.windows);
		else unread.push(monitor.name);
	});
	windows.sort((a, b) => Date.parse(a.starts_at) - Date.parse(b.starts_at));
	return { monitors: list, windows, unread, failure: null };
};
