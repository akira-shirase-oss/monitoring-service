import { redirect } from '@sveltejs/kit';
import { settle, workspace, type Membership } from '#lib/workspace.svelte.js';
import type { LayoutLoad } from './$types';

export type Account = { id: string; email: string } | null;

// The shell's one read: the caller's memberships — the workspace switcher's list and the
// session check in one — plus the account identity the foot's menu shows. 401 means the
// cookie died or never was: send the person to sign in and come back. An unreachable
// server is not a sign-out: render the shell anyway and let the page's own "cannot see"
// banner carry the truth.
//
// Both reads here are about the ACCOUNT, not about one workspace, so they are sent
// without the workspace pick. A pick the account no longer holds (it was removed from
// that workspace, or another account used this browser) would otherwise get 403 on the
// very request that could have cleared it, and lock the dashboard until site data was
// wiped.
export const load: LayoutLoad = async ({ fetch, url }) => {
	const plain = (path: string) => fetch(path, { credentials: 'same-origin' });
	try {
		const response = await plain('/api/workspaces');
		if (response.status === 401) {
			redirect(307, `/login?return=${encodeURIComponent(url.pathname)}`);
		}
		if (response.ok) {
			const body = (await response.json()) as {
				workspaces?: Membership[];
				account?: Account;
				current?: string;
			};
			workspace.list = body.workspaces ?? [];
			workspace.home = body.current ?? '';
			const stalePick = settle();
			// Invitations reach a person only here — there is no email — so the shell
			// says when one is waiting. Tokens and the operator have none (a refusal).
			let invited = 0;
			const waiting = await plain('/api/invitations');
			if (waiting.ok) {
				invited = ((await waiting.json()) as { invitations?: unknown[] }).invitations?.length ?? 0;
			}
			return {
				account: body.account ?? null,
				invited,
				// What every page's data belongs to. Pages remount when it changes, so a
				// form never carries one workspace's values into another.
				scope: workspace.picked !== '' ? workspace.picked : workspace.home,
				stalePick
			};
		}
	} catch (error) {
		if (error && typeof error === 'object' && 'status' in error) throw error;
		// network failure — render anyway; pages carry the banner
	}
	return { account: null, invited: 0, scope: '', stalePick: false };
};
