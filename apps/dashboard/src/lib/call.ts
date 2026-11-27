// One way to call the management API from a page: the session cookie and the picked
// workspace ride along (apiFetch), a failure comes back as the server's own envelope
// (readEnvelope), and an unreachable server is a failure too — never an exception a
// page forgets to catch, and never an empty list that reads like "nothing here".

import { apiFetch } from './auth.js';
import { readEnvelope, type ApiFailure } from './api.js';

export type Result<T> = { ok: true; body: T } | { ok: false; failure: ApiFailure };

/** Status 0: no answer at all. `Problem` shows it as "cannot see", not as an HTTP error. */
export const UNREACHABLE: ApiFailure = {
	status: 0,
	message: 'The monitor server is unreachable — this says nothing about your targets.'
};

export async function call<T = unknown>(
	path: string,
	init: { method?: string; body?: unknown } = {},
	fetcher: typeof fetch = fetch
): Promise<Result<T>> {
	try {
		const hasBody = init.body !== undefined;
		const response = await apiFetch(
			path,
			{
				method: init.method ?? 'GET',
				...(hasBody
					? { headers: { 'content-type': 'application/json' }, body: JSON.stringify(init.body) }
					: {})
			},
			fetcher
		);
		const failure = await readEnvelope(response);
		if (failure) return { ok: false, failure };
		const text = await response.text();
		return { ok: true, body: (text === '' ? undefined : JSON.parse(text)) as T };
	} catch {
		return { ok: false, failure: UNREACHABLE };
	}
}

/** What `Problem` takes: `null` for "no answer", else the envelope. */
export function shown(failure: ApiFailure): ApiFailure | null {
	return failure.status === 0 ? null : failure;
}
