import { beforeEach, describe, expect, it, vi } from 'vitest';
import { logout } from './auth.js';
import { pick, workspace } from './workspace.svelte.js';

beforeEach(() => {
	workspace.list = [];
	pick('ws-picked');
});

describe('logout', () => {
	it('is confirmed only by the server, and then forgets the workspace pick', async () => {
		const fetcher = vi.fn(async () => new Response(null, { status: 204 }));
		vi.stubGlobal('fetch', fetcher);
		expect(await logout()).toBe(true);
		expect(workspace.picked).toBe('');
		expect(fetcher).toHaveBeenCalledWith('/api/logout', { method: 'POST', credentials: 'same-origin' });
	});

	it('reports a server that did not answer as still signed in, and keeps the pick', async () => {
		vi.stubGlobal(
			'fetch',
			vi.fn(async () => {
				throw new TypeError('Failed to fetch');
			})
		);
		expect(await logout()).toBe(false);
		expect(workspace.picked).toBe('ws-picked');
	});

	it('reports a refusal as still signed in', async () => {
		vi.stubGlobal('fetch', vi.fn(async () => new Response('{}', { status: 503 })));
		expect(await logout()).toBe(false);
		expect(workspace.picked).toBe('ws-picked');
	});
});
