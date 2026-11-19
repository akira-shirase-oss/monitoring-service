#!/usr/bin/env node
// Screenshot harness for the Service Monitor dashboard — run OUTSIDE the dev sandbox.
// Usage (from the worktree root, after `cargo build -p monitor-server` and
// `npm --prefix apps/dashboard install && npm --prefix apps/dashboard run build`):
//   node shots/shoot.mjs [before|after]
// Writes PNGs to shots/<tag>-*.png. Uses ports 58510 (server) + 58511 (fixture),
// inside the devin-smon band 58500-58519. Never touches ~/smon or :58400.
import { spawn } from 'node:child_process';
import { mkdtempSync, mkdirSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import http from 'node:http';
import { chromium } from '@playwright/test';

const tag = process.argv[2] ?? 'after';
const PORT = 58510;
const FIXTURE = 58511;
const TOKEN = 'shoot-token-0123456789abcdef0123456789abcdef';
const root = new URL('..', import.meta.url).pathname;
const run = mkdtempSync(join(tmpdir(), 'smon-shoot-'));
const dirs = {};
for (const d of ['status', 'backups']) {
	dirs[d] = join(run, d);
	mkdirSync(dirs[d], { recursive: true });
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const fixture = http
	.createServer((_req, res) => {
		res.writeHead(200, { 'content-type': 'text/plain' });
		res.end('ok');
	})
	.listen(FIXTURE, '127.0.0.1');

const serverBin = join(root, 'target/debug/monitor-server');
const env = {
	...process.env,
	SMON_PROFILE: 'lab',
	SMON_ADMIN_TOKEN: TOKEN,
	SMON_DATABASE_URL: `sqlite:${join(run, 'monitor.db')}`,
	SMON_LISTEN: `127.0.0.1:${PORT}`,
	SMON_STATUS_DIR: dirs.status,
	SMON_BACKUP_DIR: dirs.backups,
	SMON_DASHBOARD_DIR: join(root, 'apps/dashboard/build')
};

const init = spawn(serverBin, ['--init'], { env, stdio: 'inherit' });
await new Promise((r, j) => init.on('exit', (c) => (c === 0 ? r() : j(new Error(`init ${c}`)))));
const server = spawn(serverBin, [], { env, stdio: 'inherit' });

const api = async (path, init = {}) => {
	const res = await fetch(`http://127.0.0.1:${PORT}${path}`, {
		...init,
		headers: { authorization: `Bearer ${TOKEN}`, 'content-type': 'application/json' }
	});
	if (!res.ok) throw new Error(`${path} -> ${res.status}: ${await res.text()}`);
	return res.json();
};

for (let i = 0; i < 100; i++) {
	try {
		await api('/livez');
		break;
	} catch {
		await sleep(200);
	}
}

const monitor = await api('/api/monitors', {
	method: 'POST',
	body: JSON.stringify({
		name: 'fixture ok',
		url: `http://127.0.0.1:${FIXTURE}/health`,
		interval_s: 5,
		timeout_s: 2
	})
});
await api(`/api/monitors/${monitor.id}/maintenance`, {
	method: 'POST',
	body: JSON.stringify({
		starts_at: new Date(Date.now() + 3600_000).toISOString(),
		ends_at: new Date(Date.now() + 7200_000).toISOString(),
		reason: 'screenshot seed'
	})
});
// Let a few checks land so the board shows real evidence.
await sleep(12_000);

const browser = await chromium.launch();
const ctx = await browser.newContext();
await ctx.request.post(`http://127.0.0.1:${PORT}/api/login`, { data: { token: TOKEN } });

const shots = [
	[`http://127.0.0.1:${PORT}/`, 'overview'],
	[`http://127.0.0.1:${PORT}/monitors/${monitor.id}`, 'detail'],
	[`http://127.0.0.1:${PORT}/status`, 'status']
];
for (const [w, h] of [
	[1440, 900],
	[390, 844]
]) {
	for (const theme of ['light', 'dark']) {
		for (const [url, name] of shots) {
			const page = await ctx.newPage();
			await page.setViewportSize({ width: w, height: h });
			await page.emulateMedia({ colorScheme: theme });
			await page.goto(url);
			await page.evaluate((t) => {
				document.documentElement.classList.toggle('dark', t === 'dark');
			}, theme);
			await page.waitForTimeout(1200);
			await page.screenshot({ path: join(root, 'shots', `${tag}-${name}-${w}-${theme}.png`) });
			await page.close();
		}
	}
}
await browser.close();
server.kill('SIGTERM');
fixture.close();
console.log(`wrote shots/${tag}-*.png (fixture data under ${run})`);
