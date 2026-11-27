import { call } from '#lib/call.js';
import type { PageLoad } from './$types';

// The workspace itself comes from the shell's membership list; the page adds the one
// figure the plan card needs — how many monitors the workspace has against its cap.
export const load: PageLoad = async ({ fetch }) => {
	const monitors = await call<{ monitors: unknown[] }>('/api/monitors', {}, fetch);
	return { monitorCount: monitors.ok ? monitors.body.monitors.length : null };
};
