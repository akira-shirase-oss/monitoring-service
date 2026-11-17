#!/usr/bin/env node
// Connection-limits drill (M2): what an anonymous client can do to the listener before it
// sends a complete request. Starts its own monitor-server and checks that
//   1. a request whose header never finishes is closed by the server (header-read timeout),
//   2. an idle keep-alive connection is closed by the server (same deadline),
//   3. connections past SMON_MAX_CONNECTIONS wait in the backlog instead of taking file
//      descriptors — and are served once a slot frees — while the process stays healthy.
//
//   node perf/connections.mjs [--server-port 58462] [--cap 16] [--deadline-s 15] [--out perf/results]
//
// Its own port — never 58400, where an installed monitor listens. A busy port refuses the
// run before anything starts, and it drives only the server it started (perf/lib-ports.mjs).
// Takes about 25 s. Numbers are development observations, never capacity claims.

import { spawn, execFileSync } from 'node:child_process';
import { dirname, resolve, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { mkdirSync, rmSync, writeFileSync } from 'node:fs';
import net from 'node:net';
import { requireFreePorts, waitOwnServer } from './lib-ports.mjs';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const arg = (n, d) => { const i = process.argv.indexOf(`--${n}`); return i === -1 ? d : process.argv[i + 1]; };
const SERVER_PORT = Number(arg('server-port', 58462));
const CAP = Number(arg('cap', 16));
// The server's header-read deadline is 10 s; allow scheduling slack, but not "forever".
const DEADLINE_MS = Number(arg('deadline-s', 15)) * 1000;
const OUT_DIR = resolve(ROOT, arg('out', 'perf/results'));
const DB = `/tmp/smon-conns-${process.pid}.db`;
const BASE = `http://127.0.0.1:${SERVER_PORT}`;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

await requireFreePorts([[SERVER_PORT, '--server-port']]);

/** Open a socket, write `bytes`, and resolve with how long until the server closes it
 *  (or `null` if it is still open at `limitMs`). Any bytes received are kept. */
function closedAfter(bytes, limitMs) {
	return new Promise((resolveClosed) => {
		const started = Date.now();
		const socket = net.connect({ host: '127.0.0.1', port: SERVER_PORT });
		let received = '';
		let done = false;
		const finish = (value) => {
			if (done) return;
			done = true;
			clearTimeout(timer);
			socket.destroy();
			resolveClosed({ closed_ms: value, received });
		};
		const timer = setTimeout(() => finish(null), limitMs);
		socket.on('connect', () => { if (bytes) socket.write(bytes); });
		socket.on('data', (chunk) => { received += chunk.toString(); });
		socket.on('end', () => finish(Date.now() - started));
		socket.on('close', () => finish(Date.now() - started));
		socket.on('error', () => finish(Date.now() - started));
	});
}

/** A complete request on a fresh socket; resolves with ms until the response starts, or null. */
function answeredAfter(limitMs) {
	return new Promise((resolveAnswered) => {
		const started = Date.now();
		const socket = net.connect({ host: '127.0.0.1', port: SERVER_PORT });
		const timer = setTimeout(() => { socket.destroy(); resolveAnswered(null); }, limitMs);
		socket.on('connect', () => socket.write('GET /livez HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n'));
		socket.on('data', (chunk) => {
			if (chunk.toString().startsWith('HTTP/1.1 200')) {
				clearTimeout(timer);
				socket.destroy();
				resolveAnswered(Date.now() - started);
			}
		});
		socket.on('error', () => { clearTimeout(timer); resolveAnswered(null); });
	});
}

const children = [];
const env = {
	...process.env,
	SMON_PROFILE: 'lab',
	SMON_DATABASE_URL: `sqlite:${DB}`,
	SMON_LISTEN: `127.0.0.1:${SERVER_PORT}`,
	SMON_MAX_CONNECTIONS: String(CAP),
	SMON_STATUS_DIR: `/tmp/smon-conns-status-${process.pid}`,
	SMON_DASHBOARD_DIR: '/nonexistent',
	RUST_LOG: 'monitor_server=warn',
};
let result;
try {
	execFileSync(`${ROOT}/target/debug/monitor-server`, ['--init'], { env, stdio: 'ignore' });
	const server = spawn(`${ROOT}/target/debug/monitor-server`, [], { env, stdio: 'ignore' });
	children.push(server);
	await waitOwnServer(BASE, server);

	// 1 and 2 together: a half-sent header, and a keep-alive left idle after one request.
	const [slowHeader, idleKeepAlive] = await Promise.all([
		closedAfter('GET /livez HTTP/1.1\r\nHost: 127.0.0.1\r\nX-Slow: ', DEADLINE_MS),
		closedAfter('GET /livez HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n', DEADLINE_MS),
	]);

	// 3: fill every slot with silent sockets, then ask for /livez.
	const holders = Array.from({ length: CAP }, () => closedAfter('', DEADLINE_MS));
	await sleep(300);
	const blockedWhileFull = await answeredAfter(1500);
	const servedAfterSlotsFree = await answeredAfter(DEADLINE_MS);
	const held = await Promise.all(holders);
	const stillHealthy = (await fetch(`${BASE}/livez`)).ok && server.exitCode === null;

	result = {
		date: new Date().toISOString(),
		scenario: 'connection-limits',
		cap: CAP,
		slow_header_closed_ms: slowHeader.closed_ms,
		idle_keep_alive_closed_ms: idleKeepAlive.closed_ms,
		idle_keep_alive_was_answered: idleKeepAlive.received.startsWith('HTTP/1.1 200'),
		answered_while_full_ms: blockedWhileFull,
		answered_after_slots_free_ms: servedAfterSlotsFree,
		silent_holders_closed: held.filter((h) => h.closed_ms !== null).length,
		still_healthy: stillHealthy,
		commit: execFileSync('git', ['rev-parse', '--short', 'HEAD'], { cwd: ROOT, encoding: 'utf8' }).trim(),
		machine: execFileSync('uname', ['-ms'], { encoding: 'utf8' }).trim(),
	};
	result.gates = {
		slow_header_closed: slowHeader.closed_ms !== null,
		idle_keep_alive_closed: idleKeepAlive.closed_ms !== null && result.idle_keep_alive_was_answered,
		cap_holds: blockedWhileFull === null,
		served_once_a_slot_frees: servedAfterSlotsFree !== null,
		silent_holders_closed: result.silent_holders_closed === CAP,
		still_healthy: stillHealthy,
	};
	result.passed = Object.values(result.gates).every(Boolean);
	mkdirSync(OUT_DIR, { recursive: true });
	writeFileSync(join(OUT_DIR, `connections-${result.date.replace(/[:.]/g, '')}.json`), JSON.stringify(result, null, 2));
	console.log(JSON.stringify(result, null, 2));
} finally {
	for (const child of children) child.kill('SIGTERM');
	await sleep(300);
	for (const child of children) child.kill('SIGKILL');
	for (const path of [DB, `${DB}-wal`, `${DB}-shm`]) rmSync(path, { force: true });
	rmSync(`/tmp/smon-conns-status-${process.pid}`, { recursive: true, force: true });
}
process.exit(result?.passed ? 0 : 1);
