import { redirect } from '@sveltejs/kit';
import { apiFetch } from '#lib/auth.js';
import { call } from '#lib/call.js';
import { settle, workspace, type Membership } from '#lib/workspace.svelte.js';
import type { LayoutLoad } from './$types';

export type Account = { id: string; email: string } | null;

// The shell's one read: the caller's memberships — the workspace switcher's list and the
// session check in one — plus the account identity the foot's menu shows. 401 means the
// cookie died or never was: send the person to sign in and come back. An unreachable
// server is not a sign-out: render the shell anyway and let the page's own "cannot see"
// banner carry the truth.
export const load: LayoutLoad = async ({ fetch, url }) => {
	try {
		const response = await apiFetch('/api/workspaces', undefined, fetch);
		if (response.status === 401) {
			redirect(307, `/login?return=${encodeURIComponent(url.pathname)}`);
		}
		if (response.ok) {
			const body = (await response.json()) as { workspaces?: Membership[]; account?: Account };
			workspace.list = body.workspaces ?? [];
			settle();
			// Invitations reach a person only here — there is no email — so the shell
			// says when one is waiting. Tokens and the operator have none (a refusal).
			const waiting = await call<{ invitations: unknown[] }>('/api/invitations', {}, fetch);
			return {
				account: body.account ?? null,
				invited: waiting.ok ? waiting.body.invitations.length : 0
			};
		}
	} catch (error) {
		if (error && typeof error === 'object' && 'status' in error) throw error;
		// network failure — render anyway; pages carry the banner
	}
	return { account: null, invited: 0 };
};
