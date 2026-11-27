import { beforeEach, describe, expect, it } from 'vitest';
import { current, pick, settle, workspace, type Membership } from './workspace.svelte.js';

const acme: Membership = { id: 'ws-acme', slug: 'acme', name: 'Acme owners', role: 'viewer' };
const own: Membership = { id: 'ws-own', slug: 'own', name: 'Personal workspace', role: 'owner' };

beforeEach(() => {
	workspace.switching = false;
	// Listed by name, as the server lists them: the other workspace sorts first.
	workspace.list = [acme, own];
	workspace.home = 'ws-own';
	pick('');
});

describe('current', () => {
	it("is the server's default scope when nothing is picked — not the first row by name", () => {
		expect(current()?.id).toBe('ws-own');
	});

	it('is the pick when the pick is a real membership', () => {
		pick('ws-acme');
		expect(current()?.id).toBe('ws-acme');
	});

	it('falls back to the default scope when the pick is not in the list', () => {
		pick('ws-removed');
		expect(current()?.id).toBe('ws-own');
	});

	it('uses the first row only when the server named no default', () => {
		workspace.home = '';
		expect(current()?.id).toBe('ws-acme');
	});

	it('is undefined with no memberships at all', () => {
		workspace.list = [];
		expect(current()).toBeUndefined();
	});
});

describe('settle', () => {
	it('drops a pick the account no longer holds and says it did', () => {
		pick('ws-removed');
		expect(settle()).toBe(true);
		expect(workspace.picked).toBe('');
		expect(localStorage.getItem('smon_workspace')).toBe('');
	});

	it('keeps a real pick and an empty one', () => {
		pick('ws-acme');
		expect(settle()).toBe(false);
		expect(workspace.picked).toBe('ws-acme');
		pick('');
		expect(settle()).toBe(false);
	});
});

describe('pick', () => {
	it('remembers the choice in this browser', () => {
		pick('ws-acme');
		expect(localStorage.getItem('smon_workspace')).toBe('ws-acme');
	});
});
