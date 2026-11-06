#!/usr/bin/env node
// M7.3 — the real-app load drill. The lab profile measured the monitor against canned
// fixtures; this drill asks the harder question: does the monitor tell the truth about
// a REAL application under rising load — including a degradation window and recovery?
//
//   perf/real-app.mjs   — a zero-dep Node app that does genuine work per request
//   fault-server /sink  — the webhook receiver (alert evidence)
//   monitor-server      — lab profile, real build, real SQLite
//
// Phases: ramp (closed-loop concurrency 2→40) → degrade (90% post-work 503s; incident
// must open) → recover (incident must resolve). Evidence comes from the MANAGEMENT API
// (committed check rows, incidents) plus the app's own __stats (ground truth) and the
// receiver's delivered events — never from log scraping or timing luck.
//
//   node perf/real-app-load.mjs [--ramp-s 210] [--degrade-s 120] [--recover-s 90]
//
// Numbers are development observations on this machine — never public capacity claims.

import { spawn, execFileSync } from 'node:child_process';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { mkdirSync, statSync, writeFileSync } from 'node:fs';
import http from 'node:http';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const arg = (name, dflt) => {
	const i = process.argv.indexOf(`--${name}`);
	return i === -1 ? dflt : process.argv[i + 1];
};
const RAMP_S = Number(arg('ramp-s', 210));
const DEGRADE_S = Number(arg('degrade-s', 120));
const RECOVER_S = Number(arg('recover-s', 90));
const OUT_DIR = resolve(ROOT, arg('out', 'perf/results'));
const APP_PORT = Number(arg('app-port', 58800));
const SINK_PORT = Number(arg('sink-port', 58740));
const SERVER_PORT = Number(arg('server-port', 58450));
const TOKEN = 'drill-token';
const DB = `/tmp/smon-realapp-${process.pid}.db`;
const APP = `http://127.0.0.1:${APP_PORT}`;
const BASE = `http://127.0.0.1:${SERVER_PORT}`;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const AUTH = { authorization: `Bearer ${TOKEN}`, 'content-type': 'application/json' };
const children = [];
function run(cmd, args, env = {}) {
	const p = spawn(cmd, args, { env: { ...process.env, ...env }, stdio: 'ignore' });
	children.push(p);
	return p;
}
function killAll() {
	for (const p of children) {
		try { p.kill('SIGKILL'); } catch {}
	}
}
process.on('exit', killAll);

const api = async (path, init = {}) => {
	const r = await fetch(`${BASE}${path}`, { ...init, headers: { ...AUTH, ...(init.headers ?? {}) } });
	return r;
};
const appPost = async (path, body) =>
	fetch(`${APP}${path}`, { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(body) });

function pct(sorted, p) {
	if (sorted.length === 0) return null;
	return sorted[Math.min(sorted.length - 1, Math.floor((p / 100) * sorted.length))];
}
function commit() {
	try {
		return execFileSync('git', ['rev-parse', '--short', 'HEAD'], { cwd: ROOT, encoding: 'utf8' }).trim();
	} catch { return 'unknown'; }
}

// Closed-loop load: N workers each request→await→repeat, so concurrency is a real
// queueing pressure, not a fire-and-forget flood. Ramp steps the worker count up.
const load = { workers: 0, sent: 0, errors: 0, latency: [], stop: false };
async function worker() {
	while (!load.stop) {
		const t0 = performance.now();
		try {
			const post = Math.random() < 0.3;
			const r = await fetch(
				post ? `${APP}/api/echo` : `${APP}/api/items?n=120`,
				post
					? { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ ts: Date.now(), pad: 'x'.repeat(512) }) }
					: {}
			);
			await r.arrayBuffer();
		} catch {
			load.errors += 1;
		}
		const ms = performance.now() - t0;
		load.sent += 1;
		load.latency.push(ms);
		if (load.latency.length > 8192) load.latency.shift();
	}
}
function setConcurrency(n) {
	while (load.workers < n) { load.workers += 1; worker(); }
}

async function main() {
	mkdirSync(OUT_DIR, { recursive: true });
	const started = new Date();
	run('node', [`${ROOT}/perf/real-app.mjs`, String(APP_PORT)]);
	run('node', [`${ROOT}/fixtures/fault-server.mjs`, String(SINK_PORT)]);
	const server = run(`${ROOT}/target/debug/monitor-server`, [], {
		SMON_PROFILE: 'lab',
		SMON_DATABASE_URL: `sqlite:${DB}`,
		SMON_LISTEN: `127.0.0.1:${SERVER_PORT}`,
		SMON_ADMIN_TOKEN: TOKEN,
		SMON_WEBHOOK_URL: `http://127.0.0.1:${SINK_PORT}/sink`,
		SMON_WEBHOOK_SECRET: 'realapp-secret',
		SMON_LOG_FILE: `/tmp/smon-realapp-${process.pid}.log`,
		RUST_LOG: 'monitor_server=info',
	});
	for (let i = 0; i < 60; i++) {
		try { if ((await fetch(`${BASE}/livez`)).ok) break; } catch {}
		await sleep(250);
	}

	const monitors = {};
	for (const [key, path, thresholds] of [
		['app-items', '/api/items?n=80', { failures_to_open: 2, passes_to_recover: 1 }],
		['app-health', '/health', {}],
	]) {
		const r = await api('/api/monitors', {
			method: 'POST',
			body: JSON.stringify({ name: `m73-${key}`, url: `${APP}${path}`, interval_s: 15, timeout_s: 5, ...thresholds }),
		});
		if (r.status !== 201) throw new Error(`add ${key}: HTTP ${r.status}`);
		monitors[key] = (await r.json()).id;
	}

	const rss = [];
	const sampler = setInterval(() => {
		try {
			const kb = Number(execFileSync('ps', ['-o', 'rss=', '-p', String(server.pid)], { encoding: 'utf8' }).trim());
			rss.push(kb);
		} catch {}
	}, 5000);

	const phaseMarks = [];
	const mark = (phase) => phaseMarks.push({ phase, at: new Date().toISOString() });

	// Phase 1 — ramp: workers step 2→40 across the window.
	mark('ramp');
	setConcurrency(2);
	const steps = 7;
	for (let s = 1; s <= steps; s++) {
		setConcurrency(2 + Math.round((38 * s) / steps));
		await sleep((RAMP_S * 1000) / steps);
	}

	// Phase 2 — degrade: 85% partial outage for most of the window (a pass can still
	// heal the incident mid-phase — that is the domain being honest, not a flake),
	// then a 45s hard outage so a stable-open incident reaches the recovery phase.
	mark('degrade');
	await appPost('/__ctl', { degrade: 0.85 });
	await sleep((DEGRADE_S - 45) * 1000);
	await appPost('/__ctl', { degrade: 1.0 });
	await sleep(45_000);

	// Phase 3 — recover.
	mark('recover');
	await appPost('/__ctl', { degrade: 0 });
	await sleep(RECOVER_S * 1000);

	load.stop = true;
	clearInterval(sampler);
	// Let the last slot's commit + alert delivery settle before reading evidence.
	await sleep(20_000);

	// --- Evidence: committed checks, incidents, delivered alerts, app truth ---
	const checks = {};
	for (const key of Object.keys(monitors)) {
		const r = await api(`/api/monitors/${monitors[key]}/checks?limit=400`);
		checks[key] = (await r.json()).checks ?? [];
	}
	const incidents = (await (await api('/api/incidents?limit=50')).json()).incidents ?? [];
	const events = (await (await fetch(`http://127.0.0.1:${SINK_PORT}/sink/events`)).json()).events ?? [];
	// The sink records {at, event (x-smon-event id), signature, body}: kind lives in the body.
	const eventKinds = events.map((e) => {
		try { return JSON.parse(e.body).kind ?? null; } catch { return null; }
	});
	const truth = await (await fetch(`${APP}/__stats`)).json();
	const dbBytes = statSync(DB).size;

	const inWindow = (iso, phase) => {
		const i = phaseMarks.findIndex((m) => m.phase === phase);
		const start = Date.parse(phaseMarks[i].at);
		const end = i + 1 < phaseMarks.length ? Date.parse(phaseMarks[i + 1].at) : Infinity;
		const t = Date.parse(iso);
		return t >= start && t < end;
	};
	const rowsOf = (key, phase) =>
		checks[key].filter((c) => !phase || inWindow(c.slot_at, phase));

	const degradeRows = rowsOf('app-items', 'degrade');
	const degradeFailRate = degradeRows.length
		? degradeRows.filter((c) => c.outcome !== 'pass').length / degradeRows.length
		: 0;
	const rampLat = rowsOf('app-items', 'ramp').map((c) => c.latency_ms).filter((v) => v != null).sort((a, b) => a - b);
	const truthItems = truth.routes['GET /api/items'] ?? { n: 0, errors: 0 };
	const truthErrorRate = truthItems.n ? truthItems.errors / truthItems.n : 0;
	const itemIncidents = incidents.filter((i) => i.monitor_id === monitors['app-items']);
	const openedInWindow = itemIncidents.filter(
		(i) => i.opened_at && inWindow(i.opened_at, 'degrade')
	);
	const opened = events.filter((_, i) => eventKinds[i] === 'incident_opened');
	const resolved = events.filter((_, i) => eventKinds[i] === 'incident_recovered');
	const missed = Object.values(checks).flat().filter((c) => c.outcome === 'missed').length;
	const lags = Object.values(checks).flat()
		.map((c) => Date.parse(c.recorded_at) - Date.parse(c.slot_at))
		.sort((a, b) => a - b);

	const assertions = [
		{ name: 'app served real traffic', pass: truthItems.n > 200, detail: `${truthItems.n} GET /api/items` },
		{ name: 'committed evidence ≈ slots', pass: checks['app-items'].length >= Math.floor((RAMP_S + DEGRADE_S + RECOVER_S) / 15) - 2, detail: `${checks['app-items'].length} rows` },
		{ name: 'no missed slots', pass: missed === 0, detail: `${missed}` },
		{ name: 'degrade read as http_fail', pass: degradeFailRate > 0.5, detail: `${(degradeFailRate * 100).toFixed(0)}% of degrade-window checks failed` },
		{ name: 'incident opened in the window', pass: openedInWindow.length >= 1, detail: openedInWindow.map((i) => i.opened_at).join(', ') || 'none' },
		{ name: 'every incident healed once the app did', pass: itemIncidents.length >= 1 && itemIncidents.every((i) => i.resolved_at != null), detail: `${itemIncidents.filter((i) => !i.resolved_at).length} still open` },
		{ name: 'control monitor stayed quiet', pass: !incidents.some((i) => i.monitor_id === monitors['app-health']), detail: 'no health incidents' },
		{ name: 'open alert delivered', pass: opened.length >= 1, detail: `${opened.length}` },
		{ name: 'resolve alert delivered', pass: resolved.length >= 1, detail: `${resolved.length}` },
		{ name: 'app recorded the outage in its own stats', pass: truthItems.errors > 0 && truthErrorRate > 0.15, detail: `app 5xx rate ${(truthErrorRate * 100).toFixed(1)}% over ${truthItems.n} requests` },
		{ name: 'probe latency bounded under load', pass: pct(rampLat, 95) != null && pct(rampLat, 95) < 5_000, detail: `monitor-observed ramp p95 ${pct(rampLat, 95)}ms` },
		{ name: 'commit lag p95 < 15s', pass: pct(lags, 95) != null && pct(lags, 95) < 15_000, detail: `${pct(lags, 95)}ms` },
	];

	const result = {
		run: 'real-app rising-load drill (M7.3)',
		commit: commit(),
		started_at: started.toISOString(),
		duration_s: RAMP_S + DEGRADE_S + RECOVER_S + 20,
		machine: execFileSync('uname', ['-sm'], { encoding: 'utf8' }).trim(),
		phases: phaseMarks,
		load: {
			requests_sent: load.sent,
			client_errors: load.errors,
			latency_p50_ms: Math.round(pct([...load.latency].sort((a, b) => a - b), 50) ?? 0),
			latency_p95_ms: Math.round(pct([...load.latency].sort((a, b) => a - b), 95) ?? 0),
			peak_concurrency: load.workers,
		},
		monitor: {
			rss_kb: { start: rss[0] ?? null, end: rss[rss.length - 1] ?? null, max: Math.max(...rss, 0) },
			db_bytes: dbBytes,
			checks_committed: Object.fromEntries(Object.entries(checks).map(([k, v]) => [k, v.length])),
			missed_slots: missed,
			commit_lag_ms: { p50: pct(lags, 50), p95: pct(lags, 95), max: lags[lags.length - 1] ?? null },
			incidents,
			delivered_events: events.map((e, i) => ({ kind: eventKinds[i], event_id: e.event ?? null })),
		},
		app_truth: truth,
		assertions,
		passed: assertions.every((a) => a.pass),
	};
	const out = resolve(OUT_DIR, `real-app-${started.toISOString().replaceAll(':', '-')}-${commit()}.json`);
	writeFileSync(out, JSON.stringify(result, null, 2));
	console.log(`\n=== real-app drill ===`);
	for (const a of assertions) console.log(`${a.pass ? 'PASS' : 'FAIL'}  ${a.name}  (${a.detail})`);
	console.log(`evidence: ${out}`);
	killAll();
	process.exit(result.passed ? 0 : 1);
}

main().catch((error) => {
	console.error('drill failed:', error);
	killAll();
	process.exit(1);
});
