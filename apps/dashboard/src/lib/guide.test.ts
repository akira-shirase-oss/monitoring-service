import { describe, expect, it } from 'vitest';
import { dismissGuide, guide, guideOffered } from './guide.svelte.js';

describe('the setup guide link', () => {
	it('is offered for a workspace until it is put away, per workspace', () => {
		expect(guideOffered('ws-a')).toBe(true);
		dismissGuide('ws-a');
		expect(guideOffered('ws-a')).toBe(false);
		expect(guideOffered('ws-b')).toBe(true);
	});

	it('is not offered when there is no workspace to offer it for', () => {
		expect(guideOffered(undefined)).toBe(false);
	});

	it('remembers in this browser, once per workspace', () => {
		dismissGuide('ws-c');
		dismissGuide('ws-c');
		expect(guide.dismissed.filter((id) => id === 'ws-c')).toHaveLength(1);
		expect(JSON.parse(localStorage.getItem('smon_guide_done') ?? '[]')).toContain('ws-c');
	});

	it('ignores an empty id rather than hiding the guide everywhere', () => {
		const before = guide.dismissed.length;
		dismissGuide('');
		expect(guide.dismissed).toHaveLength(before);
	});
});
