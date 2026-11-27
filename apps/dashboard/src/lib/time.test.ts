import { describe, expect, it } from 'vitest';
import { ago, fromLocalInput, percent, span, when } from './time.js';

describe('span', () => {
	it('reads as seconds, minutes, hours and days', () => {
		expect(span(0)).toBe('0 s');
		expect(span(59_400)).toBe('59 s');
		expect(span(60_000)).toBe('1 min');
		expect(span(59 * 60_000)).toBe('59 min');
		expect(span(60 * 60_000)).toBe('1 h');
		expect(span(3 * 3_600_000 + 10 * 60_000)).toBe('3 h 10 min');
		expect(span(24 * 3_600_000)).toBe('1 d');
		expect(span(53 * 3_600_000)).toBe('2 d 5 h');
	});

	it('never shows a negative span', () => {
		expect(span(-5_000)).toBe('0 s');
	});
});

describe('ago', () => {
	const now = Date.parse('2026-11-27T12:00:00Z');

	it('says never for nothing, and just now inside 45 seconds', () => {
		expect(ago(null, now)).toBe('never');
		expect(ago(undefined, now)).toBe('never');
		expect(ago('2026-11-27T11:59:30Z', now)).toBe('just now');
	});

	it('counts back in the largest sensible unit', () => {
		expect(ago('2026-11-27T11:55:00Z', now)).toBe('5 min ago');
		expect(ago('2026-11-27T09:00:00Z', now)).toBe('3 h ago');
		expect(ago('2026-11-25T12:00:00Z', now)).toBe('2 d ago');
	});

	it('shows what it was given when that is not a date', () => {
		expect(ago('not a date', now)).toBe('not a date');
	});

	it('shows the date itself for the future and for anything over a month old', () => {
		expect(ago('2026-11-27T13:00:00Z', now)).not.toContain('ago');
		expect(ago('2026-09-01T00:00:00Z', now)).not.toContain('ago');
	});
});

describe('when', () => {
	it('is a dash for nothing and the raw text for an unreadable date', () => {
		expect(when(null)).toBe('—');
		expect(when('')).toBe('—');
		expect(when('[2026,331,15]')).toBe('[2026,331,15]');
	});

	it('formats a real date instead of echoing it', () => {
		const shown = when('2026-11-27T12:00:00Z');
		expect(shown).not.toBe('2026-11-27T12:00:00Z');
		expect(shown).toContain('2026');
	});
});

describe('fromLocalInput', () => {
	it('turns a datetime-local value into an RFC 3339 instant', () => {
		const out = fromLocalInput('2026-11-28T10:00');
		expect(out).toMatch(/^2026-11-2[78]T\d{2}:00:00\.000Z$/);
		expect(new Date(out ?? '').getTime()).toBe(new Date('2026-11-28T10:00').getTime());
	});

	it('is null for an empty or unreadable value, never an invalid date', () => {
		expect(fromLocalInput('')).toBeNull();
		expect(fromLocalInput('tomorrow')).toBeNull();
	});
});

describe('percent', () => {
	it('reads basis points as a percentage', () => {
		expect(percent(10_000)).toBe('100%');
		expect(percent(9_987)).toBe('99.87%');
		expect(percent(0)).toBe('0%');
	});

	it('is a dash when there is nothing to divide by', () => {
		expect(percent(null)).toBe('—');
		expect(percent(undefined)).toBe('—');
	});
});
