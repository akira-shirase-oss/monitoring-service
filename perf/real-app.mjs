#!/usr/bin/env node
// The M7.3 drill target: a real application, not a fixture. Every route does genuine
// work — request-body streaming, JSON shaping, hashing, allocation — so its latency
// under rising load is real behavior, not a canned response. `__stats` is the app's
// own ground truth; `__ctl` simulates a bad deploy (a fraction of requests still do
// their work, then fail — the honest shape of a partial outage).
//
//   node perf/real-app.mjs [port]
//
//   GET /health          — cheap liveness
//   GET /api/items?n=K   — K items: hash + shape + serialize (real CPU per request)
//   POST /api/echo       — streams the body, validates JSON, answers with an id
//   GET /__stats         — requests served, latency samples, error counts, loop lag
//   POST /__ctl          — { degrade: 0..1, slow_ms: n } — incident injection

import http from 'node:http';
import { createHash, randomUUID } from 'node:crypto';

const port = Number(process.argv[2] ?? 58800);
const MAX_BODY = 256 * 1024;
// Degradation state — what a bad deploy looks like from the outside.
const ctl = { degrade: 0, slow_ms: 0 };
// Ground truth: what the app actually did, per route.
const stats = {
	routes: new Map(), // path -> { n, errors, latency[] (ring of last 2048) }
	started: Date.now(),
	loopLag: []
};

function record(path, ms, error) {
	const r = stats.routes.get(path) ?? { n: 0, errors: 0, latency: [] };
	r.n += 1;
	if (error) r.errors += 1;
	r.latency.push(ms);
	if (r.latency.length > 2048) r.latency.shift();
	stats.routes.set(path, r);
}

// Event-loop lag sampled every 200ms — the honest saturation signal.
let last = process.hrtime.bigint();
setInterval(() => {
	const now = process.hrtime.bigint();
	const lag = Number(now - last) / 1e6 - 200;
	last = now;
	stats.loopLag.push(Math.max(0, lag));
	if (stats.loopLag.length > 2048) stats.loopLag.shift();
}, 200).unref();

function items(n) {
	const out = [];
	for (let i = 0; i < n; i++) {
		const hash = createHash('sha256').update(`${randomUUID()}:${i}`).digest('hex');
		out.push({ id: hash.slice(0, 12), sku: `sku-${i}`, price_cents: (i * 7919) % 100_000 });
	}
	return out;
}

const server = http.createServer(async (request, response) => {
	const t0 = process.hrtime.bigint();
	const url = new URL(request.url ?? '/', `http://${request.headers.host ?? 'localhost'}`);
	const route = `${request.method} ${url.pathname}`;

	const done = (status, body) => {
		const ms = Number(process.hrtime.bigint() - t0) / 1e6;
		record(route, ms, status >= 500);
		response.writeHead(status, { 'content-type': 'application/json' });
		response.end(typeof body === 'string' ? body : JSON.stringify(body));
	};

	// A degraded deploy fails AFTER doing its work — the latency cost is real either way.
	const maybeDegrade = () => ctl.degrade > 0 && Math.random() < ctl.degrade;
	const spin = ctl.slow_ms > 0 ? new Promise((r) => setTimeout(r, ctl.slow_ms)) : null;

	if (url.pathname === '/health') {
		return done(200, { status: 'ok', uptime_s: (Date.now() - stats.started) / 1000 });
	}
	if (url.pathname === '/__stats' && request.method === 'GET') {
		const routes = {};
		for (const [path, r] of stats.routes) {
			const lat = [...r.latency].sort((a, b) => a - b);
			routes[path] = {
				n: r.n,
				errors: r.errors,
				p50: lat[Math.floor(lat.length * 0.5)] ?? null,
				p95: lat[Math.floor(lat.length * 0.95)] ?? null,
				p99: lat[Math.floor(lat.length * 0.99)] ?? null
			};
		}
		const lag = [...stats.loopLag].sort((a, b) => a - b);
		return done(200, {
			routes,
			loop_lag_p95_ms: lag[Math.floor(lag.length * 0.95)] ?? null,
			ctl
		});
	}
	if (url.pathname === '/__ctl' && request.method === 'POST') {
		let body = '';
		for await (const chunk of request) body += chunk;
		try {
			const c = JSON.parse(body || '{}');
			if (typeof c.degrade === 'number') ctl.degrade = Math.min(1, Math.max(0, c.degrade));
			if (typeof c.slow_ms === 'number') ctl.slow_ms = Math.max(0, c.slow_ms);
		} catch {}
		return done(200, { ctl });
	}
	if (url.pathname === '/api/items' && request.method === 'GET') {
		const n = Math.min(Number(url.searchParams.get('n') ?? 50), 500);
		if (spin) await spin;
		const list = items(n);
		if (maybeDegrade()) return done(503, { error: 'dependency unavailable' });
		return done(200, { items: list, count: list.length });
	}
	if (url.pathname === '/api/echo' && request.method === 'POST') {
		let size = 0;
		const chunks = [];
		for await (const chunk of request) {
			size += chunk.length;
			if (size > MAX_BODY) return done(413, { error: 'body too large' });
			chunks.push(chunk);
		}
		if (spin) await spin;
		let parsed = null;
		try {
			parsed = JSON.parse(Buffer.concat(chunks).toString('utf8') || 'null');
		} catch {
			return done(400, { error: 'invalid json' });
		}
		if (maybeDegrade()) return done(503, { error: 'dependency unavailable' });
		return done(200, { id: randomUUID(), received: parsed, bytes: size });
	}
	return done(404, { error: 'not found' });
});

server.listen(port, '127.0.0.1', () => {
	console.log(`real-app listening on ${port}`);
});
