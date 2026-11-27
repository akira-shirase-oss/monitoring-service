import { describe, expect, it } from 'vitest';
import { explainDelivery } from './alerts.js';

describe('explainDelivery', () => {
	it('explains the words the server uses', () => {
		expect(explainDelivery('delivered')).toContain('success status');
		expect(explainDelivery('rejected_local')).toContain('private or local network');
		expect(explainDelivery('deadline')).toContain('did not answer in time');
	});

	it('names the status of an HTTP failure', () => {
		expect(explainDelivery('http_500')).toBe(
			'Your endpoint answered HTTP 500, which counts as a failed delivery.'
		);
		expect(explainDelivery('http_302')).toContain('HTTP 302');
	});

	it('falls back plainly for a word it has not been taught', () => {
		expect(explainDelivery('something_new')).toBe('The delivery failed.');
		expect(explainDelivery('http_5000')).toBe('The delivery failed.');
	});
});
