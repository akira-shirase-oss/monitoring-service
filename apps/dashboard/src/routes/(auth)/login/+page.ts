import { redirect } from '@sveltejs/kit';
import type { PageLoad } from './$types';

// A live session skips the form entirely — signing in twice is a mistake, not a state.
// An unreachable server can't tell either way: show the form and let the submit say so.
export const load: PageLoad = async ({ fetch }) => {
	try {
		const response = await fetch('/api/workspaces', { credentials: 'same-origin' });
		if (response.ok) redirect(307, '/');
	} catch {
		// fall through to the form
	}
	return {};
};
