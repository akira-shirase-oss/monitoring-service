import { beforeEach, describe, expect, it, vi } from 'vitest';
import { readEnvelope } from './api.js';
import { call, shown, UNREACHABLE } from './call.js';
import { pick, workspace } from './workspace.svelte.js';

function answering(status: number, body?: unknown) {
	return vi.fn(async (_input: RequestInfo | URL, _init?: RequestInit) => {
		const text = body === undefined ? '' : typeof body === 'string' ? body : JSON.stringify(body);
		// A 204 may not carry a body at all.
		return new Response(status === 204 ? null : text, { status });
	});
}

function sent(fetcher: ReturnType<typeof answering>): { url: string; init: RequestInit; headers: Headers } {
	const [input, init] = fetcher.mock.calls[0] ?? [];
	return { url: String(input), init: init ?? {}, headers: new Headers(init?.headers) };
}

beforeEach(() => {
	workspace.list = [];
	workspace.home = '';
	pick('');
});

describe('call', () => {
	it('returns the parsed body of a successful answer', async () => {
		const fetcher = answering(200, { monitors: [{ id: 'm1' }] });
		const answer = await call<{ monitors: { id: string }[] }>('/api/monitors', {}, fetcher);
		expect(answer).toEqual({ ok: true, body: { monitors: [{ id: 'm1' }] } });
		expect(sent(fetcher).init.method).toBe('GET');
		expect(sent(fetcher).init.credentials).toBe('same-origin');
	});

	it('treats an empty success (204) as done, not as a parse error', async () => {
		const answer = await call('/api/tokens/t1', { method: 'DELETE' }, answering(204));
		expect(answer).toEqual({ ok: true, body: undefined });
	});

	it("carries the server's own words on a refusal", async () => {
		const fetcher = answering(409, {
			error: { code: 'plan_limit', message: "interval 15s is below the 60s minimum on the workspace's plan" }
		});
		const answer = await call('/api/monitors', { method: 'POST', body: { name: 'x' } }, fetcher);
		expect(answer).toEqual({
			ok: false,
			failure: { status: 409, message: "interval 15s is below the 60s minimum on the workspace's plan" }
		});
	});

	it('falls back to the status line when a refusal is not JSON', async () => {
		const answer = await call('/api/monitors', {}, answering(502, '<html>bad gateway</html>'));
		expect(answer).toEqual({ ok: false, failure: { status: 502, message: 'HTTP 502' } });
	});

	it('reports an unreachable server as a failure, never as an exception or an empty list', async () => {
		const fetcher = vi.fn(async () => {
			throw new TypeError('Failed to fetch');
		});
		const answer = await call('/api/monitors', {}, fetcher);
		expect(answer).toEqual({ ok: false, failure: UNREACHABLE });
	});

	it('sends a JSON body with its content type, and no content type without a body', async () => {
		const withBody = answering(201, { id: 'm1' });
		await call('/api/monitors', { method: 'POST', body: { name: 'api' } }, withBody);
		expect(sent(withBody).headers.get('content-type')).toBe('application/json');
		expect(sent(withBody).init.body).toBe('{"name":"api"}');

		const without = answering(200, {});
		await call('/api/monitors', {}, without);
		expect(sent(without).headers.get('content-type')).toBeNull();
		expect(sent(without).init.body).toBeUndefined();
	});

	it('names the picked workspace on every request, and nothing when none is picked', async () => {
		const unpicked = answering(200, {});
		await call('/api/monitors', {}, unpicked);
		expect(sent(unpicked).headers.get('x-smon-workspace')).toBeNull();

		pick('ws-b');
		const picked = answering(200, {});
		await call('/api/monitors', {}, picked);
		expect(sent(picked).headers.get('x-smon-workspace')).toBe('ws-b');
	});
});

describe('shown', () => {
	it('hands Problem null for "no answer" and the envelope otherwise', () => {
		expect(shown(UNREACHABLE)).toBeNull();
		expect(shown({ status: 403, message: 'no' })).toEqual({ status: 403, message: 'no' });
	});
});

describe('readEnvelope', () => {
	it('is null for a success', async () => {
		expect(await readEnvelope(new Response('{}', { status: 200 }))).toBeNull();
	});

	it('keeps the status line when the envelope has no message', async () => {
		expect(await readEnvelope(new Response('{"error":{}}', { status: 500 }))).toEqual({
			status: 500,
			message: 'HTTP 500'
		});
	});
});
