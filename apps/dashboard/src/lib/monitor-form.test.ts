import { describe, expect, it } from 'vitest';
import { statusSpecs } from './monitor-form.js';

describe('statusSpecs', () => {
	it('splits on commas and spaces', () => {
		expect(statusSpecs('200-299, 401')).toEqual(['200-299', '401']);
		expect(statusSpecs('200-204 418')).toEqual(['200-204', '418']);
		expect(statusSpecs(' 200 ,, 204 ')).toEqual(['200', '204']);
	});

	it('is undefined for empty input, so the server keeps its default', () => {
		expect(statusSpecs('')).toBeUndefined();
		expect(statusSpecs('  , ')).toBeUndefined();
	});

	it('passes a bad spec through for the server to refuse in its own words', () => {
		expect(statusSpecs('2xx')).toEqual(['2xx']);
	});
});
