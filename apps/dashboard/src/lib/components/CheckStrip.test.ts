import { render } from '@testing-library/svelte';
import { describe, expect, it } from 'vitest';
import type { StripCheck } from '#lib/check-strip.js';
import CheckStrip from './CheckStrip.svelte';

const check = (slot_at: string, outcome: string, http_status: number | null = 200): StripCheck => ({
	slot_at,
	outcome,
	http_status,
	latency_ms: 12,
	detail: null
});

describe('CheckStrip', () => {
	it('draws nothing when there are no checks — no placeholder row of green', () => {
		const { container } = render(CheckStrip, { props: { checks: [], intervalS: 15 } });
		expect(container.querySelector('.check-strip')).toBeNull();
	});

	it('draws one cell per check and a missing cell for each hole', () => {
		const { container } = render(CheckStrip, {
			props: {
				checks: [
					check('2026-11-27T12:00:00Z', 'pass'),
					check('2026-11-27T12:00:15Z', 'http_fail', 500),
					// two slots (12:00:30, 12:00:45) were never observed
					check('2026-11-27T12:01:00Z', 'pass')
				],
				intervalS: 15
			}
		});
		const cells = [...container.querySelectorAll('.check-strip .cell')];
		expect(cells.map((cell) => cell.getAttribute('data-outcome'))).toEqual([
			'pass',
			'http_fail',
			'missing',
			'pass'
		]);
		expect(cells[2]?.getAttribute('title')).toBe('2 slots not observed');
		expect(cells[1]?.getAttribute('title')).toContain('HTTP 500');
	});

	it('is labelled for a screen reader as a history, oldest first', () => {
		const { container } = render(CheckStrip, {
			props: { checks: [check('2026-11-27T12:00:00Z', 'pass')], intervalS: 15 }
		});
		expect(container.querySelector('[role="img"]')?.getAttribute('aria-label')).toBe(
			'check history, oldest to newest'
		);
	});
});
