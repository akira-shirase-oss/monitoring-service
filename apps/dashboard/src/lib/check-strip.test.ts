import { describe, expect, it } from 'vitest';
import { buildStripCells, gapLabel, type StripCheck } from './check-strip.js';

function check(at: string, outcome = 'pass'): StripCheck {
	return { slot_at: at, outcome, http_status: 200, latency_ms: 10, detail: null };
}

describe('buildStripCells', () => {
	it('is one cell per check, oldest first, whatever order they arrive in', () => {
		const cells = buildStripCells(
			[check('2026-11-27T12:00:30Z'), check('2026-11-27T12:00:00Z'), check('2026-11-27T12:00:15Z')],
			15
		);
		expect(cells.map((c) => c.kind)).toEqual(['check', 'check', 'check']);
		expect(cells.map((c) => (c.kind === 'check' ? c.check.slot_at : ''))).toEqual([
			'2026-11-27T12:00:00Z',
			'2026-11-27T12:00:15Z',
			'2026-11-27T12:00:30Z'
		]);
	});

	it('shows a hole in the evidence as a gap, never as more checks', () => {
		// 15 s interval, checks at :00 and 1:00 — three slots in between were not observed.
		const cells = buildStripCells([check('2026-11-27T12:00:00Z'), check('2026-11-27T12:01:00Z')], 15);
		expect(cells).toHaveLength(3);
		expect(cells[1]).toEqual({ kind: 'gap', slots: 3 });
	});

	it('absorbs commit jitter without inventing or hiding a slot', () => {
		const cells = buildStripCells([check('2026-11-27T12:00:00Z'), check('2026-11-27T12:00:17Z')], 15);
		expect(cells.map((c) => c.kind)).toEqual(['check', 'check']);
	});

	it('splits a long outage into several gap cells that still add up', () => {
		// 20 missing slots at 60 s.
		const cells = buildStripCells([check('2026-11-27T12:00:00Z'), check('2026-11-27T12:21:00Z')], 60);
		const gaps = cells.filter((c) => c.kind === 'gap');
		expect(gaps.map((g) => (g.kind === 'gap' ? g.slots : 0))).toEqual([8, 8, 4]);
	});

	it('draws no gaps when the interval is unknown', () => {
		const cells = buildStripCells([check('2026-11-27T12:00:00Z'), check('2026-11-27T13:00:00Z')], 0);
		expect(cells.every((c) => c.kind === 'check')).toBe(true);
	});

	it('is empty for no checks', () => {
		expect(buildStripCells([], 15)).toEqual([]);
	});
});

describe('gapLabel', () => {
	it('counts slots in words', () => {
		expect(gapLabel(1)).toBe('1 slot not observed');
		expect(gapLabel(4)).toBe('4 slots not observed');
	});
});
