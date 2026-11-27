import { call } from '#lib/call.js';
import type { PageLoad } from './$types';

export type TokenView = {
	id: string;
	name: string;
	role: 'member' | 'viewer';
	created_at: string;
	revoked_at: string | null;
	last_used_at: string | null;
};

export const load: PageLoad = async ({ fetch }) => {
	const answer = await call<{ tokens: TokenView[] }>('/api/tokens', {}, fetch);
	return answer.ok
		? { tokens: answer.body.tokens, failure: null }
		: { tokens: [] as TokenView[], failure: answer.failure };
};
