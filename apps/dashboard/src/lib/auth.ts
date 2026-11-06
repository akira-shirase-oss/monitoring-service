// Management auth (M5.1): the token lives in localStorage — entered once via the gate,
// sent as `Authorization: Bearer` on every API call. The server answers 401 when it's
// missing or wrong; the UI shows the gate again rather than guessing at data.

const KEY = 'smon-admin-token';

export function getToken(): string | null {
	if (typeof localStorage === 'undefined') return null;
	return localStorage.getItem(KEY);
}

export function setToken(token: string | null): void {
	if (token === null || token === '') {
		localStorage.removeItem(KEY);
	} else {
		localStorage.setItem(KEY, token);
	}
}

/** fetch + the Bearer header. `fetcher` lets load() pass SvelteKit's wrapped fetch. */
export function apiFetch(
	input: string | URL,
	init: RequestInit | undefined,
	fetcher: typeof fetch
): Promise<Response> {
	const headers = new Headers(init?.headers);
	const token = getToken();
	if (token) headers.set('authorization', `Bearer ${token}`);
	return fetcher(input, { ...init, headers });
}
