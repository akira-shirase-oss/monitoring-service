#!/usr/bin/env node
// Load harness — the guide's lab profile, scaled to a laptop: N monitors at the minimum
// 15 s interval across a deterministic failure mix, all registering at t=0 so the first
// wave is a thundering-herd burst on the 20 global probe slots. Measures what the module
// contract asks: dispatch/commit lag, outcome/error distribution, missed accounting,
// outbox backpressure, incident+delivery correctness, RSS growth, DB growth.
//
//   node perf/lab-profile.mjs [--duration-s 600] [--monitors 100] [--out perf/results]
//
// Numbers are development observations on this machine — never public capacity claims.

import { spawn, execFileSync } from 'node:child_process';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { mkdirSync, statSync, writeFileSync } from 'node:fs';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const arg = (name, dflt) => {
	const i = process.argv.indexOf(`--${name}`);
	return i === -1 ? dflt : process.argv[i + 1];
};
const DURATION_S = Number(arg('duration-s', 600));
const N_MONITORS = Number(arg('monitors', 100));
const OUT_DIR = resolve(ROOT, arg('out', 'perf/results'));
const FIXTURE_PORT = Number(arg('fixture-port', 58700));
// One origin can never fill the global pool: per-origin is 2, so <10 origins throttles
// everything to a trickle (that bound is the product working — the harness must not
// accidentally measure it). Spread monitors across ORIGINS fixture ports.
const ORIGINS = Number(arg('origins', 10));
const SERVER_PORT = Number(arg('server-port', 58400));
const DB = `/tmp/smon-load-${process.pid}.db`;
const LOG = `/tmp/smon-load-${process.pid}.log`;
const BASE = `http://127.0.0.1:${SERVER_PORT}`;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// Failure mix — every target class the product claims to handle honestly, assigned
// round-robin to origins so each origin sees the same blend.
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
		if (x < c.w) {
			return { name: `${c.name}-${i}`, port: FIXTURE_PORT + (i % ORIGINS), path: c.path(i) };
		}
		x -= c.w;
	}
	throw new Error('unreachable');
}

const sqlite = (sql) =>
	execFileSync('sqlite3', ['-json', DB, sql], { encoding: 'utf8', maxBuffer: 64 * 1024 * 1024 }).trim() || '[]';

function commit() {
	try {
		return execFileSync('git', ['rev-parse', '--short', 'HEAD'], {
			cwd: ROOT,
			encoding: 'utf8',
		}).trim();
	} catch {
		return 'unknown';
	}
}

function pct(sorted, p) {
	if (sorted.length === 0) return null;
	return sorted[Math.min(sorted.length - 1, Math.floor((p / 100) * sorted.length))];
}

const children = [];
function run(cmd, args, env = {}) {
	const p = spawn(cmd, args, { env: { ...process.env, ...env }, stdio: 'ignore' });
	children.push(p);
	return p;
}

async function main() {
	mkdirSync(OUT_DIR, { recursive: true });
	const started = new Date();
	for (let o = 0; o < ORIGINS; o++) {
		run('node', [`${ROOT}/fixtures/fault-server.mjs`, String(FIXTURE_PORT + o)]);
	}
	const server = run(`${ROOT}/target/debug/monitor-server`, [], {
		SMON_PROFILE: 'lab',
		SMON_DATABASE_URL: `sqlite:${DB}`,
		SMON_LISTEN: `127.0.0.1:${SERVER_PORT}`,
		SMON_WEBHOOK_URL: `http://127.0.0.1:${FIXTURE_PORT}/sink`,
		SMON_WEBHOOK_SECRET: 'loadrun-secret',
		SMON_LOG_FILE: LOG,
		SMON_LOG_MAX_BYTES: String(8 * 1024 * 1024),
		RUST_LOG: 'monitor_server=info',
	});
	for (let i = 0; i < 50; i++) {
		try {
			const r = await fetch(`${BASE}/livez`);
			if (r.ok) break;
		} catch {}
		await sleep(200);
	}

	const t0 = Date.now();
	const ids = await Promise.all(
		Array.from({ length: N_MONITORS }, async (_, i) => {
			const { name, port, path } = plan(i);
			const r = await fetch(`${BASE}/api/monitors`, {
				method: 'POST',
				headers: { 'content-type': 'application/json' },
				body: JSON.stringify({
					name,
					url: `http://127.0.0.1:${port}${path}`,
					interval_s: 15,
					timeout_s: 3,
				}),
			});
			if (r.status !== 201) throw new Error(`add ${name}: HTTP ${r.status}`);
			return (await r.json()).id;
		}),
	);
	const registered_ms = Date.now() - t0;

	const rss = [];
	const outbox = [];
	const sampler = setInterval(() => {
		try {
			const kb = Number(execFileSync('ps', ['-o', 'rss=', '-p', String(server.pid)], { encoding: 'utf8' }).trim());
			rss.push({ t: Date.now(), kb });
			const pending = JSON.parse(
				sqlite('SELECT COUNT(*) n FROM notify_outbox WHERE delivered_at IS NULL'),
			)[0]?.n ?? 0;
			outbox.push({ t: Date.now(), pending });
		} catch {}
	}, 5000);

	await sleep(DURATION_S * 1000);
	clearInterval(sampler);

	const checks = JSON.parse(
		sqlite(
			`SELECT m.name, c.outcome, c.http_status, c.detail, c.latency_ms,
			        c.slot_at, c.recorded_at
			   FROM checks c JOIN monitors m ON m.id = c.monitor_id`,
		),
	);
	const incidents = JSON.parse(sqlite('SELECT state, COUNT(*) n FROM incidents GROUP BY state'));
	const outboxRows = JSON.parse(
		sqlite(
			'SELECT attempts, delivered_at IS NOT NULL AS delivered, COUNT(*) n FROM notify_outbox GROUP BY delivered, attempts ORDER BY delivered, attempts',
		),
	);
	const sinkHits = Number(
		(await (await fetch(`http://127.0.0.1:${FIXTURE_PORT}/sink/events`)).json()).events
			.length,
	);
	const dbBytes = statSync(DB).size;
	const logBytes = statSync(LOG).size;

	// Commit lag = recorded_at − slot_at (upper bound on dispatch lag — includes the probe
	// itself); a target-independent estimate subtracts the probe's own latency.
	const ms = (s) => Date.parse(s.replace(' ', 'T') + (s.endsWith('Z') ? '' : 'Z'));
	const lags = [];
	const lagsOk = [];
	const byOutcome = {};
	for (const c of checks) {
		byOutcome[c.outcome] = (byOutcome[c.outcome] ?? 0) + 1;
		const lag = ms(c.recorded_at) - ms(c.slot_at) - (c.latency_ms ?? 0);
		lags.push(lag);
		if (c.name.startsWith('ok-')) lagsOk.push(lag);
	}
	lags.sort((a, b) => a - b);
	lagsOk.sort((a, b) => a - b);

	const rssKb = rss.map((r) => r.kb);
	const result = {
		run: {
			commit: commit(),
			machine: execFileSync('uname', ['-ms'], { encoding: 'utf8' }).trim(),
			started: started.toISOString(),
			duration_s: DURATION_S,
			monitors: N_MONITORS,
			interval_s: 15,
			timeout_s: 3,
			registered_ms,
		},
		checks: {
			total: checks.length,
			by_outcome: byOutcome,
			error_bps:
				checks.length === 0
					? null
					: Math.round(
							(1 -
								((byOutcome.pass ?? 0) + (byOutcome.missed ?? 0)) / checks.length) *
								10000,
						),
		},
		commit_lag_ms: {
			all: { p50: pct(lags, 50), p95: pct(lags, 95), max: lags.at(-1) ?? null },
			ok_targets: { p50: pct(lagsOk, 50), p95: pct(lagsOk, 95), max: lagsOk.at(-1) ?? null },
		},
		incidents,
		outbox_rows: outboxRows,
		outbox_pending_max: Math.max(0, ...outbox.map((o) => o.pending)),
		sink_hits: sinkHits,
		db_bytes: dbBytes,
		log_bytes: logBytes,
		rss_kb: { first: rssKb.at(0) ?? null, last: rssKb.at(-1) ?? null, max: Math.max(...rssKb, 0) },
	};

	const file = `${OUT_DIR}/${started.toISOString().replaceAll(':', '-')}-${commit()}.json`;
	writeFileSync(file, JSON.stringify(result, null, 2));
	console.log(JSON.stringify(result, null, 2));
	console.log(`\nwrote ${file}`);
	for (const p of children) p.kill('SIGTERM');
	await sleep(500);
	process.exit(0);
}

main().catch((e) => {
	console.error(e);
	for (const p of children) p.kill('SIGKILL');
	process.exit(1);
});
