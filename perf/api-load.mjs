#!/usr/bin/env node
// API read load — the management API at the workspace's 1,000,000-request standard while
// the probe engine runs underneath (the LOAD_MATRIX "API read surface" row).
//
//   node perf/api-load.mjs [--requests 1000000] [--vus 32] [--monitors 100] [--origins 10]
//                          [--server-port 58460] [--fixture-port 58461] [--bin debug|release]
//                          [--warmup-s 20] [--out perf/results]
//
// One lab-profile monitor-server with SMON_ADMIN_TOKEN set, so every request passes bearer
// auth and the guessing throttle's take-and-refund path; the guide's 100-monitor lab mix
// keeps probes and the writer busy beneath the reads; k6 (perf/k6/api-reads.js) sends the
// requests over keep-alive connections. `--duration-s` is accepted and ignored: the run is
// sized by `--requests`.
//
// Gate: every request answered 200 (the storm harness's api_reads_ok rule, at 1M).
// Latency and the probe engine's own numbers in the window are recorded, not gated — no
// target is defined for API latency yet.
//
// Its own ports (server 58460, fixtures 58461…58461+origins−1) — never 58400, where an
// installed monitor listens. A busy port refuses the run before anything starts, and the
// harness only drives the server it started (perf/lib-ports.mjs).
//
// Numbers are development observations on this machine — never public capacity claims.

import { spawn, execFileSync } from 'node:child_process';
import { randomBytes } from 'node:crypto';
import { mkdirSync, readFileSync, rmSync, statSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { requireFreePorts, waitOwnServer } from './lib-ports.mjs';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const arg = (n, d) => {
	const i = process.argv.indexOf(`--${n}`);
	return i === -1 ? d : process.argv[i + 1];
};
const REQUESTS = Number(arg('requests', 1_000_000));
const VUS = Number(arg('vus', 32));
const N_MONITORS = Number(arg('monitors', 100));
const ORIGINS = Number(arg('origins', 10));
const WARMUP_S = Number(arg('warmup-s', 20));
const OUT_DIR = resolve(ROOT, arg('out', 'perf/results'));
const SERVER_PORT = Number(arg('server-port', 58460));
const FIXTURE_PORT = Number(arg('fixture-port', 58461));
const BIN_PROFILE = arg('bin', 'debug');
if (!['debug', 'release'].includes(BIN_PROFILE)) throw new Error(`--bin ${BIN_PROFILE}: expected debug or release`);
const BIN =
	BIN_PROFILE === 'release' ? `${ROOT}/target/release/monitor-server` : `${ROOT}/target/debug/monitor-server`;
const DB = `/tmp/smon-api-${process.pid}.db`;
const LOG = `/tmp/smon-api-${process.pid}.log`;
const SUMMARY = `/tmp/smon-api-${process.pid}-k6.json`;
const BASE = `http://127.0.0.1:${SERVER_PORT}`;
const TOKEN = randomBytes(32).toString('hex');
const AUTH = { authorization: `Bearer ${TOKEN}` };
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const sqlite = (sql) =>
	execFileSync('sqlite3', ['-json', DB, sql], { encoding: 'utf8', maxBuffer: 64 * 1024 * 1024 }).trim() || '[]';
const loadavg = () =>
	execFileSync('sysctl', ['-n', 'vm.loadavg'], { encoding: 'utf8' }).trim().replace(/[{}]/g, '').trim().split(/\s+/).map(Number);
const pct = (sorted, p) =>
	sorted.length === 0 ? null : sorted[Math.min(sorted.length - 1, Math.floor((p / 100) * sorted.length))];

// The guide's lab mix (lab-profile.mjs), assigned round-robin across origins.
const CLASSES = [
	{ w: 50, name: 'ok', path: () => '/ok' },
	{ w: 20, name: 'flaky', path: (i) => `/flaky?fail=${1 + (i % 3)}&pass=${2 + (i % 4)}` },
	{ w: 15, name: 'slow', path: () => '/slow?ms=2000' },
	{ w: 10, name: 'dead', path: () => '/status/500' },
	{ w: 5, name: 'hung', path: () => '/hang' },
];
function plan(i) {
	let x = i % 100;
	for (const c of CLASSES) {
		if (x < c.w) return { name: `${c.name}-${i}`, port: FIXTURE_PORT + (i % ORIGINS), path: c.path(i) };
		x -= c.w;
	}
	throw new Error('unreachable');
}

const children = [];
const run = (cmd, args, env = {}, stdio = 'ignore') => {
	const p = spawn(cmd, args, { env: { ...process.env, ...env }, stdio });
	children.push(p);
	return p;
};
const cleanup = () => {
	for (const p of children) p.kill('SIGTERM');
	for (const f of [DB, `${DB}-wal`, `${DB}-shm`, LOG, SUMMARY]) rmSync(f, { force: true });
};

await requireFreePorts([
	[SERVER_PORT, '--server-port'],
	...Array.from({ length: ORIGINS }, (_, o) => [FIXTURE_PORT + o, '--fixture-port']),
]);
try {
	execFileSync('k6', ['version'], { stdio: 'ignore' });
} catch {
	console.error('k6 is not on PATH — this harness drives the API with k6 (https://k6.io)');
	process.exit(2);
}

try {
	mkdirSync(OUT_DIR, { recursive: true });
	const started = new Date();
	const loadBefore = loadavg();
	for (let o = 0; o < ORIGINS; o++) run('node', [`${ROOT}/fixtures/fault-server.mjs`, String(FIXTURE_PORT + o)]);
	// D-015: a normal start needs an existing store — create this run's fresh one first.
	const serverEnv = {
		SMON_PROFILE: 'lab',
		SMON_DATABASE_URL: `sqlite:${DB}`,
		SMON_LISTEN: `127.0.0.1:${SERVER_PORT}`,
		SMON_ADMIN_TOKEN: TOKEN,
		SMON_WEBHOOK_URL: `http://127.0.0.1:${FIXTURE_PORT}/sink`,
		SMON_WEBHOOK_SECRET: 'apiload-secret',
		SMON_LOG_FILE: LOG,
		SMON_LOG_MAX_BYTES: String(8 * 1024 * 1024),
		RUST_LOG: 'monitor_server=info',
		...(N_MONITORS > 100 ? { SMON_MAX_MONITORS: String(N_MONITORS) } : {}),
	};
	execFileSync(BIN, ['--init'], { env: { ...process.env, ...serverEnv }, stdio: 'ignore' });
	const server = run(BIN, [], serverEnv);
	await waitOwnServer(BASE, server);

	const ids = await Promise.all(
		Array.from({ length: N_MONITORS }, async (_, i) => {
			const { name, port, path } = plan(i);
			const r = await fetch(`${BASE}/api/monitors`, {
				method: 'POST',
				headers: { 'content-type': 'application/json', ...AUTH },
				body: JSON.stringify({ name, url: `http://127.0.0.1:${port}${path}`, interval_s: 15, timeout_s: 3 }),
			});
			if (r.status !== 201) throw new Error(`add ${name}: HTTP ${r.status}`);
			return (await r.json()).id;
		}),
	);
	// Let the probe engine fill the store so checks/stats/incidents reads have rows.
	await sleep(WARMUP_S * 1000);

	const samples = [];
	const k6Start = Date.now();
	const sampler = setInterval(() => {
		try {
			const kb = Number(execFileSync('ps', ['-o', 'rss=', '-p', String(server.pid)], { encoding: 'utf8' }).trim());
			samples.push({ t_s: Math.round((Date.now() - k6Start) / 1000), rss_kb: kb, loadavg: loadavg() });
		} catch {}
	}, 5000);
	const k6 = run(
		'k6',
		['run', '--quiet', '--no-color', `${ROOT}/perf/k6/api-reads.js`],
		{
			SM_BASE: BASE,
			SM_TOKEN: TOKEN,
			SM_IDS: ids.join(','),
			SM_REQUESTS: String(REQUESTS),
			SM_VUS: String(VUS),
			SM_SUMMARY: SUMMARY,
		},
		['ignore', 'ignore', 'pipe'],
	);
	let k6Stderr = '';
	k6.stderr.on('data', (d) => {
		if (k6Stderr.length < 4000) k6Stderr += d;
	});
	const k6Exit = await new Promise((r) => k6.on('exit', (code) => r(code)));
	const k6End = Date.now();
	clearInterval(sampler);

	const summary = JSON.parse(readFileSync(SUMMARY, 'utf8'));
	const m = summary.metrics;
	const trend = (metric) => {
		const v = metric?.values ?? {};
		return { min: v.min, p50: v.med, p90: v['p(90)'], p95: v['p(95)'], p99: v['p(99)'], max: v.max, avg: v.avg };
	};
	const routes = Object.fromEntries(
		Object.keys(m)
			.filter((k) => k.startsWith('http_req_duration{name:'))
			.map((k) => [k.slice('http_req_duration{name:'.length, -1), trend(m[k])]),
	);

	// The probe engine during the k6 window: what it committed, what it missed, its lag.
	const ms = (s) => Date.parse(s.replace(' ', 'T') + (s.endsWith('Z') ? '' : 'Z'));
	const rows = JSON.parse(sqlite('SELECT outcome, latency_ms, slot_at, recorded_at FROM checks')).filter(
		(c) => ms(c.slot_at) >= k6Start && ms(c.slot_at) <= k6End,
	);
	const byOutcome = {};
	const lags = [];
	for (const c of rows) {
		byOutcome[c.outcome] = (byOutcome[c.outcome] ?? 0) + 1;
		lags.push(ms(c.recorded_at) - ms(c.slot_at) - (c.latency_ms ?? 0));
	}
	lags.sort((a, b) => a - b);

	const failed = m.http_req_failed?.values ?? {};
	const reqs = m.http_reqs?.values ?? {};
	const status200 = summary.root_group?.checks?.find?.((c) => c.name === 'status 200') ??
		Object.values(summary.root_group?.checks ?? {}).find((c) => c.name === 'status 200');
	const commit = execFileSync('git', ['rev-parse', '--short', 'HEAD'], { cwd: ROOT, encoding: 'utf8' }).trim();
	const result = {
		run: {
			scenario: 'api-reads',
			commit,
			machine: execFileSync('uname', ['-ms'], { encoding: 'utf8' }).trim(),
			started: started.toISOString(),
			bin: BIN_PROFILE,
			requests_target: REQUESTS,
			vus: VUS,
			monitors: N_MONITORS,
			origins: ORIGINS,
			warmup_s: WARMUP_S,
			k6_wall_s: Math.round((k6End - k6Start) / 100) / 10,
			k6_exit: k6Exit,
			loadavg_before: loadBefore,
			loadavg_after: loadavg(),
		},
		api: {
			requests: reqs.count ?? null,
			rps: reqs.rate == null ? null : Math.round(reqs.rate),
			failed: failed.passes ?? null,
			failed_rate: failed.rate ?? null,
			status_200: status200 ? { passes: status200.passes, fails: status200.fails } : null,
			latency_ms: trend(m.http_req_duration),
			by_route_ms: routes,
		},
		probes_during_window: {
			checks: rows.length,
			by_outcome: byOutcome,
			missed: byOutcome.missed ?? 0,
			commit_lag_ms: { p50: pct(lags, 50), p95: pct(lags, 95), max: lags.at(-1) ?? null },
		},
		server_rss_kb: {
			first: samples.at(0)?.rss_kb ?? null,
			last: samples.at(-1)?.rss_kb ?? null,
			max: Math.max(0, ...samples.map((s) => s.rss_kb)),
		},
		samples,
		db_bytes: statSync(DB).size,
		k6_stderr: k6Stderr || null,
	};
	result.gates = {
		every_request_sent: (reqs.count ?? 0) >= REQUESTS,
		zero_failed: (failed.passes ?? 1) === 0 && (status200?.fails ?? 1) === 0,
	};
	result.passed = Object.values(result.gates).every(Boolean);
	const file = join(OUT_DIR, `api-load-${started.toISOString().replace(/[:.]/g, '')}-${commit}.json`);
	writeFileSync(file, JSON.stringify(result, null, 2) + '\n');
	console.log(JSON.stringify({ api: { ...result.api, by_route_ms: undefined }, probes: result.probes_during_window, gates: result.gates }, null, 2));
	console.log(result.passed ? 'PASS' : 'FAIL', `\nwrote ${file}`);
} finally {
	cleanup();
}
