// Session auth (M7.2): the dashboard trades the admin token for an HttpOnly
// `smon_session` cookie at /api/login. Nothing is stored in the browser — no
// localStorage token to steal or forget; the cookie rides same-origin requests
// on its own and the server decides when it dies (store-side expiry + logout).

/** Exchange the admin token for a session cookie. 204 sets it; 401 is a wrong token. */
export async function login(token: string): Promise<Response> {
	return fetch('/api/login', {
		method: 'POST',
		headers: { 'content-type': 'application/json' },
		body: JSON.stringify({ token }),
		credentials: 'same-origin'
	});
}

/** Delete the session row and expire the cookie — idempotent, safe to always call. */
export async function logout(): Promise<void> {
	try {
		await fetch('/api/logout', { method: 'POST', credentials: 'same-origin' });
	} catch {
		// The cookie expires anyway; a network blip shouldn't trap the sign-out.
	}
}

/**
 * fetch + the session cookie. `fetcher` lets load() pass SvelteKit's wrapped fetch.
 * `credentials: 'same-origin'` is the default for same-origin fetches — stated
 * anyway so the auth model is visible at the call site.
 */
export function apiFetch(
	input: string | URL,
	init: RequestInit | undefined,
	fetcher: typeof fetch
): Promise<Response> {
	return fetcher(input, { ...init, credentials: 'same-origin' });
}
