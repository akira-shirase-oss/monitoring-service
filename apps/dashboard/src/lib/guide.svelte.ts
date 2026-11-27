// Whether the "Get started" guide is still offered in the sidebar. Putting it away is a
// preference per workspace, kept in this browser: it hides a link, it grants nothing.

const KEY = 'smon_guide_done';

function stored(): string[] {
	try {
		const raw = JSON.parse(localStorage.getItem(KEY) ?? '[]') as unknown;
		return Array.isArray(raw) ? raw.filter((x): x is string => typeof x === 'string') : [];
	} catch {
		return [];
	}
}

export const guide = $state({ dismissed: stored() });

export function dismissGuide(workspaceId: string): void {
	if (workspaceId === '' || guide.dismissed.includes(workspaceId)) return;
	guide.dismissed = [...guide.dismissed, workspaceId];
	try {
		localStorage.setItem(KEY, JSON.stringify(guide.dismissed));
	} catch {
		// A refused write only means the link comes back next visit.
	}
}

export function guideOffered(workspaceId: string | undefined): boolean {
	return workspaceId !== undefined && !guide.dismissed.includes(workspaceId);
}
