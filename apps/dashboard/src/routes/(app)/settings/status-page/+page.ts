import { call } from '#lib/call.js';
import type { PageLoad } from './$types';

export type StatusPageView = { slug: string; title: string | null; monitor_ids: string[] };
export type MonitorName = { id: string; name: string; url: string };

export const load: PageLoad = async ({ fetch }) => {
	const [page, monitors] = await Promise.all([
		call<{ status_page: StatusPageView }>('/api/workspace/status-page', {}, fetch),
		call<{ monitors: MonitorName[] }>('/api/monitors', {}, fetch)
	]);
	if (!page.ok) return { page: null, monitors: [] as MonitorName[], failure: page.failure };
	if (!monitors.ok) return { page: null, monitors: [] as MonitorName[], failure: monitors.failure };
	return { page: page.body.status_page, monitors: monitors.body.monitors, failure: null };
};
