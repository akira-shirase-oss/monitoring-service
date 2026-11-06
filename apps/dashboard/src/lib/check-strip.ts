// Cell model for the check-history strip. The honest contract: one cell per committed
// check, and *gap* cells where the slot grid shows observations that simply do not
// exist — a hole in evidence renders as a hole, never interpolated green.

export type StripCheck = {
	slot_at: string;
	outcome: string;
	http_status: number | null;
	latency_ms: number | null;
	detail: string | null;
};

export type StripCell =
	| { kind: 'check'; check: StripCheck }
	| { kind: 'gap'; slots: number };

// How many missing slots one gap cell may summarize before another gap cell is
// emitted — keeps a long outage from becoming a wall of identical empty cells while
// still showing every missing-slot count in the tooltip.
const MAX_GAP_SLOTS_PER_CELL = 8;

export function buildStripCells(checks: StripCheck[], intervalS: number): StripCell[] {
	const sorted = [...checks].sort(
		(a, b) => new Date(a.slot_at).getTime() - new Date(b.slot_at).getTime()
	);
	const cells: StripCell[] = [];
	let previous: number | null = null;
	for (const check of sorted) {
		const at = new Date(check.slot_at).getTime();
		if (previous !== null && intervalS > 0) {
			// Slots are interval-aligned — distance in whole intervals minus the
			// observed one is the missing-slot count. A 1.5-interval tolerance absorbs
			// commit-lag jitter without ever masking a real hole.
			const skipped =
				Math.round((at - previous) / (intervalS * 1000)) - 1;
			for (let missing = skipped; missing > 0; ) {
				const chunk = Math.min(missing, MAX_GAP_SLOTS_PER_CELL);
				cells.push({ kind: 'gap', slots: chunk });
				missing -= chunk;
			}
		}
		cells.push({ kind: 'check', check });
		previous = at;
	}
	return cells;
}

export function gapLabel(slots: number): string {
	return slots === 1 ? '1 slot not observed' : `${slots} slots not observed`;
}
