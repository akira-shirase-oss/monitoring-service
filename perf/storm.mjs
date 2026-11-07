#!/usr/bin/env node
// Incident storm + API-read load — the two load gaps lab-profile doesn't cover.
//   node perf/storm.mjs [--duration-s 300] [--out perf/results]
// Storm: 40 monitors die together -> mass incidents + outbox flood while a surviving
// "ok" monitor must keep getting probe slots (delivery must never starve probes).
// Reads: sustained GET /api/monitors + /status.json beside the storm.
// Numbers are development observations on this machine — never public capacity claims.
import { spawn, execFileSync } from 'node:child_process';
import { mkdirSync, writeFileSync } from 'node:fs';
import { dirname, resolve, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const arg = (n, d) => { const i = process.argv.indexOf(`--${n}`); return i === -1 ? d : process.argv[i + 1]; };
const DURATION_S = Number(arg('duration-s', 300));
const OUT_DIR = resolve(ROOT, arg('out', 'perf/results'));
const FIXTURE_PORT = 58720, SERVER_PORT = 58420;
const DB = `/tmp/smon-storm-${process.pid}.db`, LOG = `/tmp/smon-storm-${process.pid}.log`;
const BASE = `http://127.0.0.1:${SERVER_PORT}`;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const sqlite = (sql) => execFileSync('sqlite3', ['-json', DB, sql], { encoding: 'utf8' }).trim() || '[]';
const children = [];
const run = (cmd, args, env = {}) => { const p = spawn(cmd, args, { env: { ...process.env, ...env }, stdio: 'ignore' }); children.push(p); return p; };
const DEAD = 40, ORIGINS = 10, SURVIVOR_PORT = FIXTURE_PORT + ORIGINS;

try {
	mkdirSync(OUT_DIR, { recursive: true });
	for (let o = 0; o <= ORIGINS; o++) run('node', [`${ROOT}/fixtures/fault-server.mjs`, String(FIXTURE_PORT + o)]);
	const server = run(`${ROOT}/target/debug/monitor-server`, [], {
		SMON_PROFILE: 'lab', SMON_DATABASE_URL: `sqlite:${DB}`, SMON_LISTEN: `127.0.0.1:${SERVER_PORT}`,
		SMON_WEBHOOK_URL: `http://127.0.0.1:${FIXTURE_PORT}/sink`, SMON_WEBHOOK_SECRET: 'storm-secret',
		SMON_LOG_FILE: LOG, SMON_LOG_MAX_BYTES: String(8 * 1024 * 1024), RUST_LOG: 'monitor_server=info',
	});
	for (let i = 0; i < 50; i++) { try { if ((await fetch(`${BASE}/livez`)).ok) break; } catch {} await sleep(200); }

	// One survivor (healthy) + DEAD dead monitors, all at the minimum interval.
	const add = async (name, url) => {
		const r = await fetch(`${BASE}/api/monitors`, { method: 'POST', headers: { 'content-type': 'application/json' },
			body: JSON.stringify({ name, url, interval_s: 15, timeout_s: 3 }) });
		if (r.status !== 201) throw new Error(`add ${name}: ${r.status}`);
		return (await r.json()).id;
	};
	const survivor = await add('survivor', `http://127.0.0.1:${SURVIVOR_PORT}/ok`);
	for (let i = 0; i < DEAD; i++) await add(`dead-${i}`, `http://127.0.0.1:${FIXTURE_PORT + (i % ORIGINS)}/status/500`);

	// API reads at sustained rate while the storm brews.
	let reads = 0, readFails = 0;
	const stop = Date.now() + DURATION_S * 1000;
	const reader = (async () => {
		while (Date.now() < stop) {
			const r = await fetch(`${BASE}/api/monitors`);
			r.ok ? reads++ : readFails++;
			const s = await fetch(`${BASE}/status.json`);
			if (!s.ok) readFails++;
			await sleep(50);
		}
	})();

	const t0 = Date.now(); const survivorship = [];
	const timer = setInterval(async () => {
		const open = JSON.parse(sqlite('SELECT COUNT(*) n FROM incidents WHERE resolved_at IS NULL'))[0]?.n ?? 0;
		const outbox = JSON.parse(sqlite('SELECT COUNT(*) n FROM notify_outbox WHERE delivered_at IS NULL'))[0]?.n ?? 0;
		const checks = JSON.parse(sqlite(`SELECT COUNT(*) n FROM checks WHERE monitor_id = '${survivor}'`))[0]?.n ?? 0;
		survivorship.push({ t: (Date.now() - t0) / 1000, open_incidents: open, outbox_pending: outbox, survivor_checks: checks });
	}, 5000);
	await sleep(DURATION_S * 1000);
	clearInterval(timer); await reader;

	const last = survivorship.at(-1);
	const result = {
		date: new Date().toISOString(), scenario: 'incident-storm', duration_s: DURATION_S,
		monitors: { dead: DEAD, surviving: 1 }, api_reads: reads, api_read_failures: readFails,
		final: last,
		storm_series: survivorship.filter((_, i) => i % 6 === 0),
		machine: 'apple-m2-pro-12c',
		commit: execFileSync('git', ['rev-parse', '--short', 'HEAD'], { cwd: ROOT, encoding: 'utf8' }).trim(),
		gates: {
			incidents_opened: last.open_incidents >= DEAD,
			survivor_kept_checking: last.survivor_checks >= Math.floor(DURATION_S / 15) - 1,
			api_reads_ok: readFails === 0 && reads > 0,
		},
	};
	result.passed = Object.values(result.gates).every(Boolean);
	writeFileSync(join(OUT_DIR, `storm-${result.date.replace(/[:.]/g, '')}.json`), JSON.stringify(result, null, 2));
	console.log(JSON.stringify(result.gates), `reads=${reads} incidents=${last.open_incidents} survivor_checks=${last.survivor_checks}`, result.passed ? 'PASS' : 'FAIL');
} finally { children.forEach((c) => c.kill('SIGKILL')); }
