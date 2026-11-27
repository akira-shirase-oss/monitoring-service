// The workspace the UI is scoped to (S2): what GET /api/workspaces answered, and which
// row the switcher picked. `picked` rides every apiFetch as X-Smon-Workspace; absent,
// the server serves the account's home workspace — the correct default. The pick is a
// preference, not a credential: it lives in localStorage and a stale value is cleared
// the next time the memberships load (the server only ever answers for real ones).

/** What the workspace's plan allows. `null` means the plan sets no figure of its own. */
export type Limits = {
	max_monitors: number | null;
	min_interval_s: number;
	check_retention_days: number | null;
	incident_retention_days: number | null;
};

export type Membership = {
	id: string;
	slug: string;
	name: string;
	role: string;
	plan?: string;
	limits?: Limits | null;
};

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
	picked: stored(),
	/** The workspace the server serves when no pick is sent (`current` of a header-less read). */
	home: '',
	/**
	 * True from the moment another workspace is picked until its data has arrived. The
	 * pick takes effect on the very next request, while the page on screen still shows
	 * the previous workspace's values; the shell holds the page still for that moment,
	 * so nothing typed for one workspace can be saved into the other.
	 */
	switching: false
});

export function pick(id: string): void {
	workspace.picked = id;
	try {
		localStorage.setItem(KEY, id);
	} catch {
		// A private-mode store refusal costs the memory of the pick, nothing else.
	}
}

/**
 * Keep `picked` honest: a membership the list no longer holds can only 403. Returns
 * whether a stale pick was dropped, so the shell can re-read pages that were asked
 * for with it.
 */
export function settle(): boolean {
	if (workspace.picked !== '' && !workspace.list.some((m) => m.id === workspace.picked)) {
		pick('');
		return true;
	}
	return false;
}

/**
 * The workspace every request is really served from: the pick when it is a real
 * membership, else the server's own default. Never "the first row of the list" — the
 * list is ordered by name, and showing one workspace while writing to another is how
 * a webhook or a rename lands in the wrong place.
 */
export function current(): Membership | undefined {
	return (
		workspace.list.find((m) => m.id === workspace.picked) ??
		workspace.list.find((m) => m.id === workspace.home) ??
		workspace.list[0]
	);
}
