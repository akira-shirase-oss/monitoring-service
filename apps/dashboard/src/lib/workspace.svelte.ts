// The workspace the UI is scoped to (S2): what GET /api/workspaces answered, and which
// row the switcher picked. `picked` rides every apiFetch as X-Smon-Workspace; absent,
// the server serves the account's home workspace — the correct default. The pick is a
// preference, not a credential: it lives in localStorage and a stale value is cleared
// the next time the memberships load (the server only ever answers for real ones).

export type Membership = { id: string; slug: string; name: string; role: string };

const KEY = 'smon_workspace';

function stored(): string {
	try {
		return localStorage.getItem(KEY) ?? '';
	} catch {
		return '';
	}
}

export const workspace = $state({
	list: [] as Membership[],
	picked: stored()
});

export function pick(id: string): void {
	workspace.picked = id;
	try {
		localStorage.setItem(KEY, id);
	} catch {
		// A private-mode store refusal costs the memory of the pick, nothing else.
	}
}

/** Keep `picked` honest: a membership the list no longer holds can only 403. */
export function settle(): void {
	if (workspace.picked !== '' && !workspace.list.some((m) => m.id === workspace.picked)) {
		pick('');
	}
}

/** The workspace the header shows: the pick when it's real, else the home row. */
export function current(): Membership | undefined {
	return workspace.list.find((m) => m.id === workspace.picked) ?? workspace.list[0];
}
