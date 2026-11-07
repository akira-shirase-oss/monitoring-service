#!/usr/bin/env node
// Session churn — login -> authed API calls -> logout at rate, beside live probes.
//   node perf/session-churn.mjs [--duration-s 300] [--out perf/results]
// Auth profile: SMON_ADMIN_TOKEN set, so /api/* needs the session cookie — the lab's
// unauthenticated mode can't measure this. Gates: logins succeed, authed reads clean,
// unauthed refused, sessions table bounded by the sweep, probes never starved.
import { spawn, execFileSync } from 'node:child_process';
import { mkdirSync, writeFileSync } from 'node:fs';
import { dirname, resolve, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const arg = (n, d) => { const i = process.argv.indexOf(`--${n}`); return i === -1 ? d : process.argv[i + 1]; };
const DURATION_S = Number(arg('duration-s', 300));
const OUT_DIR = resolve(ROOT, arg('out', 'perf/results'));
const FIXTURE_PORT = 58740, SERVER_PORT = 58440;
const DB = `/tmp/smon-churn-${process.pid}.db`, LOG = `/tmp/smon-churn-${process.pid}.log`;
const BASE = `http://127.0.0.1:${SERVER_PORT}`, TOKEN = 'churn-secret';
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const sqlite = (sql) => execFileSync('sqlite3', ['-json', DB, sql], { encoding: 'utf8' }).trim() || '[]';
const children = [];
const run = (cmd, args, env = {}) => { const p = spawn(cmd, args, { env: { ...process.env, ...env }, stdio: 'ignore' }); children.push(p); return p; };
const WORKERS = 8;

try {
	mkdirSync(OUT_DIR, { recursive: true });
	run('node', [`${ROOT}/fixtures/fault-server.mjs`, String(FIXTURE_PORT)]);
	run(`${ROOT}/target/debug/monitor-server`, [], {
		SMON_PROFILE: 'lab', SMON_DATABASE_URL: `sqlite:${DB}`, SMON_LISTEN: `127.0.0.1:${SERVER_PORT}`,
		SMON_ADMIN_TOKEN: TOKEN, SMON_LOG_FILE: LOG, RUST_LOG: 'monitor_server=info',
	});
	for (let i = 0; i < 50; i++) { try { if ((await fetch(`${BASE}/livez`)).ok) break; } catch {} await sleep(200); }

	// A live monitor on its own origin — churn must never starve its slots.
	const login = async () => {
		const r = await fetch(`${BASE}/api/login`, { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ token: TOKEN }) });
		const cookie = r.headers.get('set-cookie')?.split(';')[0];
		return r.status === 204 && cookie?.startsWith('smon_session=') ? cookie : null;
	};
	const c = await login();
	const r = await fetch(`${BASE}/api/monitors`, { method: 'POST', headers: { 'content-type': 'application/json', cookie: c, origin: BASE },
		body: JSON.stringify({ name: 'survivor', url: `http://127.0.0.1:${FIXTURE_PORT}/ok`, interval_s: 15, timeout_s: 3 }) });
	if (r.status !== 201) throw new Error(`seed monitor: ${r.status}`);
	const survivor = (await r.json()).id;

	let logins = 0, loginFails = 0, reads = 0, readFails = 0, refused = 0, badLogins = 0;
	const stop = Date.now() + DURATION_S * 1000;
	await Promise.all(Array.from({ length: WORKERS }, async (_, w) => {
		while (Date.now() < stop) {
			const cookie = await login();
			if (!cookie) { loginFails++; continue; }
			logins++;
			const mr = await fetch(`${BASE}/api/monitors`, { headers: { cookie } });
			mr.ok ? reads++ : readFails++;
			await fetch(`${BASE}/api/logout`, { method: 'POST', headers: { cookie, origin: BASE } });
			if (w === 0) { // worker 0 also probes the negative paths at a slower cadence
				const bad = await fetch(`${BASE}/api/login`, { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ token: 'wrong' }) });
				if (bad.status === 401) badLogins++;
				const bare = await fetch(`${BASE}/api/monitors`);
				if (bare.status === 401 || bare.status === 403) refused++;
			}
		}
	}));

	const sessions = JSON.parse(sqlite('SELECT COUNT(*) n FROM sessions'))[0]?.n ?? -1;
	const checks = JSON.parse(sqlite(`SELECT COUNT(*) n FROM checks WHERE monitor_id = '${survivor}'`))[0]?.n ?? 0;
	const result = {
		date: new Date().toISOString(), scenario: 'session-churn', duration_s: DURATION_S,
		logins, login_fails: loginFails, authed_reads: reads, read_fails: readFails,
		bad_token_401s: badLogins, unauthed_refusals: refused,
		sessions_rows: sessions, survivor_checks: checks,
		machine: 'apple-m2-pro-12c',
		commit: execFileSync('git', ['rev-parse', '--short', 'HEAD'], { cwd: ROOT, encoding: 'utf8' }).trim(),
		gates: {
			logins_work: logins > 0 && loginFails === 0,
			authed_reads_ok: reads > 0 && readFails === 0,
			negatives_enforced: badLogins > 0 && refused > 0,
			sessions_bounded: sessions >= 0 && sessions < WORKERS * 4,
			probes_unstarved: checks >= Math.floor(DURATION_S / 15) - 1,
		},
	};
	result.passed = Object.values(result.gates).every(Boolean);
	writeFileSync(join(OUT_DIR, `session-churn-${result.date.replace(/[:.]/g, '')}.json`), JSON.stringify(result, null, 2));
	console.log(JSON.stringify(result.gates), `logins=${logins} reads=${reads} sessions=${sessions} checks=${checks}`, result.passed ? 'PASS' : 'FAIL');
} finally { children.forEach((ch) => ch.kill('SIGKILL')); }
