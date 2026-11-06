// Deterministic failure fixtures for the monitor — the probe contract's adversaries made
// concrete. Zero dependencies so the fixture itself can never be the reason a test is red.
//
//   node fixtures/fault-server.mjs [port]
//
// Routes:
//   /ok                    -> 200 immediately
//   /fail                  -> 500 immediately
//   /status/<n>            -> HTTP <n> immediately (400..599), anything else -> 400
//   /slow?ms=<n>           -> 200 after <n> milliseconds
//   /flaky?fail=<f>&pass=<p> -> <f> consecutive 500s, then <p> consecutive 200s, repeating
//   /hang                  -> accepts the request and never answers; the socket stays open
//   /reset                 -> clears /flaky counters
//   POST /sink             -> a webhook receiver: records the body+headers, answers 200
//   POST /sink?fail=<n>    -> like /sink but the first <n> POSTs get 500 (retry fixture)
//   POST /sink/hang        -> a webhook receiver that never answers (deadline fixture)
//   GET  /sink/events      -> the recorded deliveries as JSON, newest last
//
// Contract notes this exists to test:
//   - a probe deadline must bound the WHOLE attempt (/hang, /slow)
//   - HTTP failure is evidence about the target, transport is evidence about the network,
//     timeout is evidence about this observation — the fixture produces each cleanly
//   - 3xx is a failure, not a follow: /status/301 exists so a probe that wrongly follows
//     redirects is detectable

import http from 'node:http';

const port = Number(process.argv[2] ?? 58700);
const flaky = new Map(); // path -> { remaining }
const sink = { events: [] };

function flakyPhase(url) {
	const fail = Number(url.searchParams.get('fail') ?? 2);
	const pass = Number(url.searchParams.get('pass') ?? 1);
	const key = `${url.pathname}?fail=${fail}&pass=${pass}`;
	const state = flaky.get(key) ?? { served: 0 };
	const cycle = fail + pass;
	const phase = state.served % cycle;
	state.served += 1;
	flaky.set(key, state);
	return phase < fail;
}

const server = http.createServer((request, response) => {
	const url = new URL(request.url ?? '/', `http://${request.headers.host ?? 'localhost'}`);

	if (url.pathname === '/sink/events' && request.method === 'GET') {
		response
			.writeHead(200, { 'content-type': 'application/json' })
			.end(JSON.stringify({ events: sink.events }));
		return;
	}
	if (url.pathname.startsWith('/sink')) {
		if (url.pathname === '/sink/hang') {
			return; // a receiver that never answers
		}
		const chunks = [];
		request.on('data', (chunk) => chunks.push(chunk));
		request.on('end', () => {
			sink.events.push({
				at: new Date().toISOString(),
				event: request.headers['x-smon-event'] ?? null,
				signature: request.headers['x-smon-signature'] ?? null,
				body: Buffer.concat(chunks).toString('utf8')
			});
			const failNext = Number(url.searchParams.get('fail') ?? 0);
			if (failNext > 0 && sink.events.length <= failNext) {
				response.writeHead(500).end('sink rejecting');
				return;
			}
			response.writeHead(200).end('stored');
		});
		return;
	}
	if (url.pathname === '/hang') {
		// Never answer. Keep the socket open so a probe that forgot its deadline hangs forever.
		return;
	}
	if (url.pathname === '/redirect') {
		// ?to=/path 302s there; no param 302s to itself — a loop for hop-limit proof.
		const to = url.searchParams.get('to') ?? url.pathname + url.search;
		response.writeHead(302, { location: to }).end(`redirect -> ${to}`);
		return;
	}
	if (url.pathname === '/ok') {
		response.writeHead(200, { 'content-type': 'text/plain' }).end('ok');
		return;
	}
	if (url.pathname === '/fail') {
		response.writeHead(500, { 'content-type': 'text/plain' }).end('internal error');
		return;
	}
	if (url.pathname === '/reset') {
		flaky.clear();
		sink.events.length = 0;
		response.writeHead(200).end('reset');
		return;
	}
	if (url.pathname === '/slow') {
		const ms = Math.min(Math.max(Number(url.searchParams.get('ms') ?? 3000), 0), 60_000);
		setTimeout(() => {
			response.writeHead(200, { 'content-type': 'text/plain' }).end(`ok after ${ms}ms`);
		}, ms).unref();
		return;
	}
	if (url.pathname === '/flaky') {
		const failing = flakyPhase(url);
		console.log(`${new Date().toISOString()} flaky -> ${failing ? 500 : 200} (${request.headers['user-agent'] ?? '-'})`);
		if (failing) {
			response.writeHead(500).end('flaky: failing phase');
		} else {
			response.writeHead(200).end('flaky: passing phase');
		}
		return;
	}
	if (url.pathname.startsWith('/status/')) {
		const code = Number(url.pathname.slice('/status/'.length));
		if (Number.isInteger(code) && code >= 100 && code <= 599) {
			response.writeHead(code).end(`status ${code}`);
			return;
		}
	}
	response.writeHead(404).end(`no fixture for ${url.pathname}`);
});

// Hanging sockets would otherwise pin the process open.
server.keepAliveTimeout = 5000;
server.listen(port, '127.0.0.1', () => {
	console.log(`fault-server listening on http://127.0.0.1:${port}`);
});
