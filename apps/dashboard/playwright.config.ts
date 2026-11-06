import { fileURLToPath } from 'node:url';
import { defineConfig, devices } from '@playwright/test';

// Playwright runs the real monitor-server (serving the built dashboard on its own origin)
// plus the fault fixture, on e2e-only ports — a run never touches a dev server or its db.
const SERVER_PORT = Number(process.env['SMON_E2E_SERVER_PORT'] ?? 58410);
const FAULT_PORT = Number(process.env['SMON_E2E_FAULT_PORT'] ?? 58710);
const repo = fileURLToPath(new URL('../../', import.meta.url));
const database = `sqlite:/tmp/smon-e2e-${process.pid}.db`;

export default defineConfig({
	testDir: 'tests',
	// One server, one fixture, one database: specs share them and run in order.
	workers: 1,
	fullyParallel: false,
	retries: 0,
	forbidOnly: Boolean(process.env['CI']),
	reporter: [['list']],
	// The incident spec waits on a real 3-failure sequence at a 15s interval.
	timeout: 120_000,
	use: {
		baseURL: `http://127.0.0.1:${SERVER_PORT}`,
		trace: 'retain-on-failure'
	},
	projects: [{ name: 'chromium', use: { ...devices['Desktop Chrome'] } }],
	webServer: [
		{
			command: `node ${repo}fixtures/fault-server.mjs ${FAULT_PORT}`,
			url: `http://127.0.0.1:${FAULT_PORT}/ok`,
			reuseExistingServer: false,
			timeout: 15_000
		},
		{
			command: `${repo}target/debug/monitor-server`,
			env: {
				SMON_DATABASE_URL: database,
				SMON_LISTEN: `127.0.0.1:${SERVER_PORT}`,
				SMON_PROFILE: 'lab',
				SMON_DASHBOARD_DIR: `${repo}apps/dashboard/build`,
				SMON_STATUS_DIR: `/tmp/smon-e2e-status-${process.pid}`,
				// M7.2: e2e runs the real auth path — the dashboard earns its session
				// cookie exactly like a deployed instance.
				SMON_ADMIN_TOKEN: 'e2e-token'
			},
			url: `http://127.0.0.1:${SERVER_PORT}/livez`,
			reuseExistingServer: false,
			timeout: 30_000
		}
	]
});
