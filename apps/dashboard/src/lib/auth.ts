// Session auth: the dashboard trades a credential for an HttpOnly `__Host-smon_session`
// cookie — email+password for accounts (S1b), the admin token for the operator.
// Nothing is stored in the browser — no localStorage token to steal or forget; the
// cookie rides same-origin requests on its own and the server decides when it dies
// (store-side expiry + logout). X-Smon-Workspace picks which workspace the API
// answers for (S2); the membership list, not the header, is the authority.

import { pick, workspace } from './workspace.svelte.js';

/** Account sign-in: {email, password}. 204 sets the cookie; 401 is a bad pair. */
export async function login(email: string, password: string): Promise<Response> {
	return fetch('/api/login', {
		method: 'POST',
		headers: { 'content-type': 'application/json' },
		body: JSON.stringify({ email, password }),
		credentials: 'same-origin'
	});
}

/** Operator sign-in: the admin token alone. 204 sets the cookie; 401 is a wrong token. */
export async function loginToken(token: string): Promise<Response> {
	return fetch('/api/login', {
		method: 'POST',
		headers: { 'content-type': 'application/json' },
		body: JSON.stringify({ token }),
		credentials: 'same-origin'
	});
}

/** Invite-gated signup: 201 mints the account + its home workspace in one transaction. */
export async function signup(email: string, password: string, invite: string): Promise<Response> {
	return fetch('/api/signup', {
		method: 'POST',
		headers: { 'content-type': 'application/json' },
		body: JSON.stringify({ email, password, invite }),
		credentials: 'same-origin'
	});
}

/**
 * End the session on the server. True when the server confirmed it. False when it did
 * not answer or refused: the session is then still alive, and the caller must say so
 * rather than show a sign-in screen over a browser that is still signed in.
 */
export async function logout(): Promise<boolean> {
	try {
		const response = await fetch('/api/logout', { method: 'POST', credentials: 'same-origin' });
		if (!response.ok) return false;
	} catch {
		return false;
	}
	// The pick belongs to the account that made it: the next one to sign in on this
	// browser starts in its own workspace.
	pick('');
	return true;
}

/**
 * fetch + the session cookie + the picked workspace header. `fetcher` lets load()
 * pass SvelteKit's wrapped fetch. `credentials: 'same-origin'` is the default for
 * same-origin fetches — stated anyway so the auth model is visible at the call site.
 */
export function apiFetch(
	input: string | URL,
	init: RequestInit | undefined,
	fetcher: typeof fetch
): Promise<Response> {
	const headers = new Headers(init?.headers);
	if (workspace.picked !== '') headers.set('X-Smon-Workspace', workspace.picked);
	return fetcher(input, { ...init, headers, credentials: 'same-origin' });
}
