import { redirect } from '@sveltejs/kit';
import type { PageLoad } from './$types';

// Same rule as /login: a live session has no business on the signup card.
export const load: PageLoad = async ({ fetch }) => {
	try {
		const response = await fetch('/api/workspaces', { credentials: 'same-origin' });
		if (response.ok) redirect(307, '/');
	} catch {
		// fall through to the form
	}
	return {};
};
