import { render, screen } from '@testing-library/svelte';
import { describe, expect, it } from 'vitest';
import MonitorBadge from './MonitorBadge.svelte';

describe('MonitorBadge', () => {
	it('says the status as a word, never by colour alone', () => {
		for (const status of ['up', 'down', 'unknown'] as const) {
			const { unmount } = render(MonitorBadge, { props: { status } });
			const badge = screen.getByTestId('monitor-badge');
			expect(badge.textContent?.trim()).toBe(status);
			expect(badge.getAttribute('data-state')).toBe(status);
			unmount();
		}
	});

	it('says paused over any status: a paused monitor that last passed is not up', () => {
		render(MonitorBadge, { props: { status: 'up', paused: true } });
		const badge = screen.getByTestId('monitor-badge');
		expect(badge.textContent?.trim()).toBe('paused');
		expect(badge.getAttribute('data-state')).toBe('paused');
	});
});
