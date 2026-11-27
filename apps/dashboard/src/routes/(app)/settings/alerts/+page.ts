import { call } from '#lib/call.js';
import type { PageLoad } from './$types';

export type WebhookView = { url: string; secret: string };

export const load: PageLoad = async ({ fetch }) => {
	const answer = await call<{ webhook: WebhookView | null }>('/api/workspace/webhook', {}, fetch);
	return answer.ok
		? { webhook: answer.body.webhook, failure: null }
		: { webhook: null, failure: answer.failure };
};
