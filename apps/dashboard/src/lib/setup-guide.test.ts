import { describe, expect, it } from 'vitest';
import { DEFAULT_WORKSPACE_NAME, guideSteps } from './setup-guide.js';

const empty = { workspaceName: DEFAULT_WORKSPACE_NAME, monitorCount: 0, webhookUrl: null, observed: 0 };

describe('guideSteps', () => {
	it('ticks nothing for a brand new workspace', () => {
		expect(guideSteps(empty)).toEqual({
			named: false,
			watching: false,
			alerting: false,
			observed: false,
			complete: false
		});
	});

	it('ticks each step from the thing it stands for', () => {
		expect(guideSteps({ ...empty, workspaceName: 'Acme' }).named).toBe(true);
		expect(guideSteps({ ...empty, monitorCount: 1 }).watching).toBe(true);
		expect(guideSteps({ ...empty, webhookUrl: 'https://hooks.example.com/x' }).alerting).toBe(true);
		expect(guideSteps({ ...empty, observed: 3 }).observed).toBe(true);
	});

	it('is complete with a monitor, an alert channel and a first check — a name is optional', () => {
		const done = { ...empty, monitorCount: 2, webhookUrl: 'https://hooks.example.com/x', observed: 1 };
		expect(guideSteps(done).complete).toBe(true);
		expect(guideSteps(done).named).toBe(false);
	});

	it('is not complete while any of the three is missing', () => {
		const done = { ...empty, monitorCount: 1, webhookUrl: 'https://hooks.example.com/x', observed: 1 };
		expect(guideSteps({ ...done, monitorCount: 0 }).complete).toBe(false);
		expect(guideSteps({ ...done, webhookUrl: null }).complete).toBe(false);
		expect(guideSteps({ ...done, observed: 0 }).complete).toBe(false);
	});

	it('treats a missing name as the default, not as renamed', () => {
		expect(guideSteps({ ...empty, workspaceName: undefined }).named).toBe(false);
	});
});
