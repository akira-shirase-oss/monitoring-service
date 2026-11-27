import { call } from '#lib/call.js';
import type { PageLoad } from './$types';

export type Role = 'owner' | 'member' | 'viewer';
export type MemberView = { account: string; email: string; role: Role; created_at: string };
/** An invitation this workspace sent that hasn't been answered yet. */
export type SentInvitation = { id: string; email: string; role: Role; created_at: string };
/** An invitation addressed to the signed-in account, from another workspace. */
export type ReceivedInvitation = { id: string; workspace: string; role: Role; created_at: string };

export const load: PageLoad = async ({ fetch }) => {
	const [members, sent, received] = await Promise.all([
		call<{ members: MemberView[] }>('/api/members', {}, fetch),
		call<{ invitations: SentInvitation[] }>('/api/members/invitations', {}, fetch),
		call<{ invitations: ReceivedInvitation[] }>('/api/invitations', {}, fetch)
	]);
	return {
		members: members.ok ? members.body.members : ([] as MemberView[]),
		failure: members.ok ? null : members.failure,
		// Reading the pending list is the owner's, and invitations addressed to "me" only
		// exist for an account (not a token or the operator): a refusal on either is a
		// role answer, not an error to show.
		sent: sent.ok ? sent.body.invitations : ([] as SentInvitation[]),
		received: received.ok ? received.body.invitations : ([] as ReceivedInvitation[])
	};
};
