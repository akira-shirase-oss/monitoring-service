import { describe, expect, it } from 'vitest';
import { isFresh, toneFor, verdictFor, type PublicService, type PublicSnapshot } from './public-status.js';

function service(status: PublicService['status'], name = 'api'): PublicService {
	return { name, status, incident_open_since: null, coverage_bps: 10_000, success_bps: 10_000 };
}

const NOW = Date.parse('2026-11-27T12:00:00Z');
function snapshot(over: Partial<PublicSnapshot> = {}): PublicSnapshot {
	return {
		generated_at: '2026-11-27T11:59:30Z',
		expires_at: '2026-11-27T12:01:00Z',
		monitoring: 'available',
		services: [],
		...over
	};
}

describe('isFresh', () => {
	it('is fresh while the expiry is ahead and the monitor was observing', () => {
		expect(isFresh(snapshot(), NOW)).toBe(true);
	});

	it('is stale once the expiry has passed', () => {
		expect(isFresh(snapshot({ expires_at: '2026-11-27T11:59:59Z' }), NOW)).toBe(false);
	});

	it('is stale when the monitor said it was not observing, whatever the expiry', () => {
		expect(isFresh(snapshot({ monitoring: 'unavailable' }), NOW)).toBe(false);
	});

	it('is stale when the expiry cannot be read — an unreadable date proves nothing', () => {
		expect(isFresh(snapshot({ expires_at: 'soon' }), NOW)).toBe(false);
		expect(isFresh(snapshot({ expires_at: '' }), NOW)).toBe(false);
	});

	it('treats a snapshot without the monitoring field as observing (older writers)', () => {
		const { monitoring: _dropped, ...older } = snapshot();
		expect(isFresh(older, NOW)).toBe(true);
	});
});

describe('verdictFor', () => {
	it('is green only when every listed service is up on a fresh snapshot', () => {
		const verdict = verdictFor('ok', [service('up'), service('up', 'web')]);
		expect(verdict.tone).toBe('live');
		expect(verdict.line).toBe('All listed services are up');
	});

	it('counts what is down, singular and plural', () => {
		expect(verdictFor('ok', [service('down'), service('up')]).line).toBe('1 service is down');
		expect(verdictFor('ok', [service('down'), service('down'), service('unknown')]).line).toBe(
			'2 services are down'
		);
		expect(verdictFor('ok', [service('down')]).tone).toBe('attention');
	});

	it('is never green while any service is unknown', () => {
		const verdict = verdictFor('ok', [service('up'), service('unknown')]);
		expect(verdict.tone).toBe('draft');
		expect(verdict.line).toBe('Some services have no recent result');
	});

	it('says monitoring is unavailable on a stale snapshot, even if every row reads up', () => {
		const verdict = verdictFor('stale', [service('up')]);
		expect(verdict.tone).toBe('draft');
		expect(verdict.line).toBe('Monitoring unavailable');
	});

	it('says the status service did not answer when it did not', () => {
		expect(verdictFor('unreachable', []).line).toBe('Status unavailable');
	});

	it('says nothing is published for an empty page — not that all is well', () => {
		const verdict = verdictFor('ok', []);
		expect(verdict.tone).toBe('draft');
		expect(verdict.line).toBe('Nothing published yet');
	});
});

describe('toneFor', () => {
	it('gives each status its tone on a fresh page', () => {
		expect(toneFor('ok', 'up')).toBe('live');
		expect(toneFor('ok', 'down')).toBe('attention');
		expect(toneFor('ok', 'unknown')).toBe('draft');
	});

	it('never shows a live colour on a stale page', () => {
		expect(toneFor('stale', 'up')).toBe('draft');
		expect(toneFor('stale', 'down')).toBe('draft');
	});
});
