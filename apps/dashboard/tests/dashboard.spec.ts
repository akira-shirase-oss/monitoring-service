import { expect, test, type APIRequestContext, type BrowserContext, type Page } from '@playwright/test';
import * as fs from 'node:fs';

const FAULT = process.env['SMON_E2E_FAULT_PORT'] ?? '58710';
const TOKEN = 'e2e-token-for-the-playwright-suite-0001';
// Direct API calls use the CLI contract: Bearer, no Origin needed.
const AUTH = { authorization: `Bearer ${TOKEN}` };

// The dashboard earns its session the way a browser does: POST /api/login once,
// the HttpOnly cookie moves into the page's jar. API-side `request` calls carry
// the same cookie for reads; writes take the bearer path above.
test.beforeEach(async ({ request, context }) => {
	const response = await request.post('/api/login', { data: { token: TOKEN } });
	expect(response.status()).toBe(204);
	const state = await request.storageState();
	await context.addCookies(state.cookies);
});

async function addMonitor(
	request: APIRequestContext,
	name: string,
	url: string
): Promise<string> {
	const response = await request.post('/api/monitors', {
		data: { name, url, interval_s: 15, timeout_s: 5 },
		headers: AUTH
	});
	expect(response.status()).toBe(201);
	return (await response.json()).id as string;
}

test('an unreachable API shows "cannot see", never a green board', async ({ page }) => {
	// The dashboard HTML loads (it is a static asset); the API answering it fails.
	await page.route('**/api/**', (route) => route.abort());
	await page.goto('/');
	await expect(page.getByText('The monitor server is unreachable.')).toBeVisible();
	// Nothing fabricated: no monitor rows, no statuses.
	await expect(page.getByText('No monitors yet.')).not.toBeVisible();
});

test('the engine verdict shows as a banner — degraded and unavailable, never hidden', async ({
	page
}) => {
	// The verdict rides inside GET /api/monitors (`monitoring`): degraded keeps serving,
	// unavailable means every status reads unknown. The API is real; only the verdict
	// field is shaped, the way a stopped supporting task would shape it.
	const shape = async (monitoring: object) => {
		await page.route('**/api/monitors', async (route) => {
			if (route.request().method() !== 'GET') return route.continue();
			const response = await route.fetch();
			const body = await response.json();
			return route.fulfill({ response, json: { ...body, monitoring } });
		});
	};

	await shape({ status: 'degraded', faults: ['snapshot writer stopped'] });
	await page.goto('/');
	await expect(page.getByText('Monitoring is degraded.')).toBeVisible();
	await expect(page.getByText(/snapshot writer stopped/)).toBeVisible();

	await page.unroute('**/api/monitors');
	await shape({ status: 'unavailable', faults: [] });
	await page.goto('/');
	await expect(page.getByText('Monitoring is unavailable.')).toBeVisible();
	await expect(page.getByText(/every status below reads unknown/)).toBeVisible();
	// An unreachable server is a different truth — it must not show as engine-down.
	await expect(page.getByText('The monitor server is unreachable.')).not.toBeVisible();
});

test('a monitor registers, probes land, and the badge turns up', async ({
	page,
	request
}) => {
	const name = `e2e-up-${Date.now()}`;
	await addMonitor(request, name, `http://127.0.0.1:${FAULT}/ok`);
	await page.goto('/');
	await expect(page.getByRole('link', { name })).toBeVisible();

	// First probe lands on the next 15s boundary; status starts `unknown` and must not
	// be painted up before a check commits. Poll the API — the badge follows evidence.
	await expect
		.poll(async () => {
			const response = await request.get('/api/monitors');
			const monitors = (await response.json()).monitors;
			return monitors.find((m: { name: string }) => m.name === name)?.status;
		}, { timeout: 40_000 })
		.toBe('up');
	await page.reload();
	await expect(page.getByRole('link', { name })).toBeVisible();

	// The uptime strip renders a live cell per committed check.
	await page.getByRole('link', { name }).click();
	await expect(page.locator('.check-strip [data-outcome="pass"]').first()).toBeVisible();
});

test('three failures open an incident and the detail page tells the truth', async ({
	page,
	request
}) => {
	const name = `e2e-down-${Date.now()}`;
	const id = await addMonitor(request, name, `http://127.0.0.1:${FAULT}/fail`);

	// 3 consecutive HTTP-500 probes at a 15s interval → incident in ~45s. Wait on the
	// committed incident, not on a timer.
	await expect
		.poll(
			async () => {
				const response = await request.get(`/api/incidents?monitor_id=${id}&state=open`);
				return (await response.json()).incidents.length;
			},
			{ timeout: 75_000 }
		)
		.toBe(1);

	await page.goto(`/monitors/${id}`);
	// Detail page: down badge, evidence strip with http_fail rows, the open incident.
	await expect(page.getByRole('heading', { name })).toBeVisible();
	await expect(page.getByText('down').first()).toBeVisible();
	await expect(page.getByText('Recent checks')).toBeVisible();
	await expect(page.getByText('HTTP 500').first()).toBeVisible();
	await expect(page.getByRole('heading', { name: 'Incidents' })).toBeVisible();
	await expect(page.getByText('still open').first()).toBeVisible();
	// Coverage vs success are separate claims — success reads 0%, coverage is a number.
	await expect(page.getByText(/coverage \d+\.\d%[\s\S]*success 0\.0%/)).toBeVisible();
	// The strip shows the failing run as down cells — count tracks committed rows.
	await expect(page.locator('.check-strip [data-outcome="http_fail"]').first()).toBeVisible();
});

test('mutations through the real API: create in UI, pause, edit, delete', async ({
	page,
	request
}) => {
	// Create through the form — the API contract validates and the list shows the row.
	await page.goto('/');
	await page.getByRole('button', { name: 'Add monitor' }).click();
	const name = `e2e-mut-${Date.now()}`;
	await page.getByPlaceholder('billing-api').fill(name);
	await page
		.getByPlaceholder('https://example.com/healthz')
		.fill(`http://127.0.0.1:${FAULT}/ok`);
	await page.getByRole('button', { name: 'Create monitor' }).click();
	await expect(page.getByRole('link', { name })).toBeVisible();

	// Detail → pause: badge + API agree (the write went through, not just the view).
	await page.getByRole('link', { name }).click();
	await expect(page.getByRole('heading', { name })).toBeVisible();
	await page.getByRole('button', { name: 'Pause' }).click();
	await expect(page.getByText('paused').first()).toBeVisible();
	await expect
		.poll(async () => {
			const r = await request.get('/api/monitors');
			return (await r.json()).monitors.find(
				(m: { name: string }) => m.name === name
			)?.paused;
		})
		.toBe(true);

	// Edit: the form seeds every field, identity included; every management write bumps
	// generation once — create(1) → pause(2) → edit(3).
	await page.getByRole('button', { name: 'Edit' }).click();
	const renamed = `${name}-renamed`;
	await page.getByPlaceholder('billing-api').fill(renamed);
	await page
		.getByPlaceholder('https://example.com/healthz')
		.fill(`http://127.0.0.1:${FAULT}/ok`);
	await page.getByPlaceholder('60').fill('30');
	await page.getByRole('button', { name: 'Save changes' }).click();
	await expect(page.getByText(/every 30s/)).toBeVisible();
	await expect(page.getByText(/generation 3/)).toBeVisible();
	await expect(page.getByRole('heading', { name: renamed })).toBeVisible();
	// The API agrees the identity fields went through — the PATCH wasn't view-only.
	await expect
		.poll(async () => {
			const r = await request.get('/api/monitors');
			const m = (await r.json()).monitors.find(
				(m: { name: string }) => m.name === renamed
			);
			return m ? `${m.interval_s}` : null;
		})
		.toBe('30');

	// Delete asks first, in a dialog that names what goes with the monitor.
	await page.getByTestId('monitor-delete').click();
	await expect(page.getByTestId('confirm')).toContainText('checks, incidents');
	await page.getByTestId('confirm-action').click();
	await expect(page).toHaveURL(/\/$/);
	await expect(page.getByRole('link', { name: renamed })).not.toBeVisible();
});

test('a monitor with history is deleted with it, only after a confirmation that says so', async ({
	page,
	request
}) => {
	const name = `e2e-del-${Date.now()}`;
	const id = await addMonitor(request, name, `http://127.0.0.1:${FAULT}/ok`);
	// Wait for committed evidence: from here a plain delete refuses.
	await expect
		.poll(
			async () => {
				const r = await request.get(`/api/monitors/${id}/checks?limit=1`);
				return (await r.json()).checks.length;
			},
			{ timeout: 40_000 }
		)
		.toBeGreaterThan(0);

	// The API's default is unchanged: no client deletes history without saying so.
	const plain = await request.delete(`/api/monitors/${id}`, { headers: AUTH });
	expect(plain.status()).toBe(409);
	expect((await plain.json()).error.message).toMatch(/recorded history/);

	// Cancelling the dialog deletes nothing.
	await page.goto(`/monitors/${id}`);
	await page.getByTestId('monitor-delete').click();
	await page.getByRole('button', { name: 'Cancel' }).click();
	await expect(page.getByTestId('confirm')).toHaveCount(0);
	await expect(page.getByRole('heading', { name, exact: true })).toBeVisible();
	expect((await request.get(`/api/monitors/${id}`)).status()).toBe(200);

	// Confirming does: the monitor and its checks are gone, and the board says so.
	await page.getByTestId('monitor-delete').click();
	await page.getByTestId('confirm-action').click();
	await expect(page).toHaveURL(/\/$/);
	await expect(page.getByRole('link', { name })).toHaveCount(0);
	expect((await request.get(`/api/monitors/${id}`)).status()).toBe(404);
});

test('a paused window renders as missing slots, never interpolated green', async ({
	page,
	request
}) => {
	const name = `e2e-gap-${Date.now()}`;
	const id = await addMonitor(request, name, `http://127.0.0.1:${FAULT}/ok`);
	const checkCount = async () => {
		const r = await request.get(`/api/monitors/${id}/checks?limit=50`);
		return (await r.json()).checks.length;
	};
	// One check lands, then pause for ~2 slots, then resume — the pause window is
	// unobserved time: no rows at all, so the strip must show a gap, not cells.
	await expect.poll(checkCount, { timeout: 40_000 }).toBeGreaterThan(0);
	await request.patch(`/api/monitors/${id}`, { data: { paused: true }, headers: AUTH });
	await new Promise((resolve) => setTimeout(resolve, 35_000));
	await request.patch(`/api/monitors/${id}`, { data: { paused: false }, headers: AUTH });
	const after = await checkCount();
	await expect.poll(checkCount, { timeout: 40_000 }).toBeGreaterThan(after);

	await page.goto(`/monitors/${id}`);
	await expect(page.locator('.check-strip [data-outcome="missing"]').first()).toBeVisible();
	await expect(page.locator('.check-strip [data-outcome="pass"]').first()).toBeVisible();
});

test('auto-refresh picks up new monitors without a reload', async ({
	page,
	request
}) => {
	await page.goto('/');
	await expect(page.getByRole('heading', { name: 'Monitors' })).toBeVisible();
	const name = `e2e-refresh-${Date.now()}`;
	await addMonitor(request, name, `http://127.0.0.1:${FAULT}/ok`);
	// The next poll tick (~15s) must surface it — no navigation, no reload.
	await expect(page.getByRole('link', { name })).toBeVisible({ timeout: 25_000 });
});

test('an API outage mid-poll resolves to "cannot see", not a frozen board', async ({
	page
}) => {
	await page.goto('/');
	await expect(page.getByRole('heading', { name: 'Monitors' })).toBeVisible();
	// Kill the API after the page loaded; the next poll tick must resolve to the
	// honest unreachable state — data swaps atomically, never a stale green.
	await page.route('**/api/**', (route) => route.abort());
	await expect(
		page.getByText('The monitor server is unreachable.')
	).toBeVisible({ timeout: 25_000 });
});

test('the in-app status view renders the public snapshot', async ({
	page,
	request
}) => {
	const name = `e2e-status-${Date.now()}`;
	await addMonitor(request, name, `http://127.0.0.1:${FAULT}/ok`);
	// The snapshot writer beats every 30s — wait for the new service to appear in the
	// public artifact, not the management API.
	await expect
		.poll(
			async () => {
				const r = await request.get('/status.json');
				if (!r.ok()) return 0;
				return ((await r.json()).services ?? []).length;
			},
			{ timeout: 45_000 }
		)
		.toBeGreaterThan(0);

	await page.goto('/status');
	await expect(page.getByRole('heading', { name: 'Service status' })).toBeVisible();
	// Every schema field lands: hashed alias, status badge, snapshot meta line.
	await expect(page.locator('span.font-mono').first()).toBeVisible();
	await expect(page.getByText(/snapshot written \d+s ago/)).toBeVisible();
	await expect(page.getByText(/^(up|down|unknown)$/).first()).toBeVisible();
});

test('a stale snapshot reads "monitoring unavailable", rows stay stale evidence', async ({
	page
}) => {
	const stale = {
		generated_at: new Date(Date.now() - 120_000).toISOString(),
		expires_at: new Date(Date.now() - 30_000).toISOString(),
		services: [{ alias: 'deadbeef0123', status: 'down', incident_open_since: null }]
	};
	await page.route('**/status.json', (route) =>
		route.fulfill({
			status: 200,
			contentType: 'application/json',
			body: JSON.stringify(stale)
		})
	);
	await page.goto('/status');
	await expect(
		page.getByText('Monitoring unavailable — this snapshot is stale.')
	).toBeVisible();
	await expect(page.getByText('deadbeef0123')).toBeVisible();
	await expect(page.getByText('down').first()).toBeVisible();
});

test('no readable snapshot is "monitoring unavailable", never fabricated rows', async ({
	page
}) => {
	await page.route('**/status.json', (route) =>
		route.fulfill({ status: 404, body: 'not found' })
	);
	await page.goto('/status');
	await expect(page.getByText('Monitoring unavailable.')).toBeVisible();
	// No service rows are fabricated — the only "status" on screen is the page title.
	await expect(page.locator('span.font-mono')).toHaveCount(0);
});

test('the guide page documents every wire word in plain English', async ({
	page
}) => {
	await page.goto('/guide');
	await expect(page.getByRole('heading', { name: 'Guide' })).toBeVisible();
	for (const section of [
		'Monitor status',
		'Check outcomes',
		'Failure detail',
		'Incident vocabulary',
		'Concepts'
	]) {
		await expect(page.getByRole('heading', { name: section })).toBeVisible();
	}
	// Spot-check the words a user actually sees — wire strings rendered as code.
	for (const word of ['up', 'unknown', 'http_fail', 'missed', 'cert_expired', 'first_fail_at']) {
		await expect(page.locator('code').getByText(word, { exact: true })).toBeVisible();
	}
});

test('a mutation 401 surfaces the token error, never silent', async ({ page }) => {
	await page.route('**/api/monitors', (route) => {
		if (route.request().method() === 'POST') {
			route.fulfill({
				status: 401,
				contentType: 'application/json',
				body: JSON.stringify({
					error: { code: 'unauthorized', message: 'missing credential' }
				})
			});
		} else {
			route.continue();
		}
	});
	await page.goto('/');
	await page.getByRole('button', { name: 'Add monitor' }).click();
	await page.getByPlaceholder('billing-api').fill('e2e-401');
	await page
		.getByPlaceholder('https://example.com/healthz')
		.fill('http://127.0.0.1/healthz');
	await page.getByRole('button', { name: 'Create monitor' }).click();
	await expect(page.getByText('Your session ended — sign in again.')).toBeVisible();
});

test('an active maintenance window is visible, never silent', async ({ page, request }) => {
	const name = `e2e-window-${Date.now()}`;
	const id = await addMonitor(request, name, `http://127.0.0.1:${FAULT}/fail`);
	const now = Date.now();
	const response = await request.post(`/api/monitors/${id}/maintenance`, {
		data: {
			starts_at: new Date(now - 60_000).toISOString(),
			ends_at: new Date(now + 3_600_000).toISOString(),
			reason: 'e2e deploy rehearsal'
		},
		headers: AUTH
	});
	expect(response.status()).toBe(201);

	await page.goto(`/monitors/${id}`);
	await expect(page.getByText('Maintenance windows')).toBeVisible();
	await expect(page.getByText('suppressing now')).toBeVisible();
	await expect(page.getByText('e2e deploy rehearsal')).toBeVisible();
});

test('maintenance windows add and cancel through the UI', async ({ page, request }) => {
	const name = `e2e-mw-${Date.now()}`;
	const id = await addMonitor(request, name, `http://127.0.0.1:${FAULT}/ok`);
	await page.goto(`/monitors/${id}`);
	await expect(
		page.getByText('None set — a window suppresses incident alerts')
	).toBeVisible();

	// "from" empty means the next slot; "until" is the required bound.
	await page.getByRole('button', { name: 'Add window' }).click();
	const until = new Date(Date.now() + 3_600_000);
	const local = new Date(until.getTime() - until.getTimezoneOffset() * 60_000)
		.toISOString()
		.slice(0, 16);
	await page.locator('input[type="datetime-local"]').nth(1).fill(local);
	await page.getByPlaceholder('deploy rehearsal').fill('e2e ui window');
	await page.getByRole('button', { name: 'Add window' }).click();
	await expect(page.getByText('e2e ui window')).toBeVisible();
	await expect(page.getByText('suppressing now')).toBeVisible();
	await expect
		.poll(async () => {
			const r = await request.get(`/api/monitors/${id}/maintenance`);
			return (await r.json()).windows.length;
		})
		.toBe(1);

	// Cancelling is a two-click confirm — and the API really drops the window.
	await page.getByRole('button', { name: 'Cancel' }).click();
	await page.getByRole('button', { name: 'Confirm cancel' }).click();
	await expect(
		page.getByText('None set — a window suppresses incident alerts')
	).toBeVisible();
	await expect
		.poll(async () => {
			const r = await request.get(`/api/monitors/${id}/maintenance`);
			return (await r.json()).windows.length;
		})
		.toBe(0);
});

test('back up now writes a server-named file inside SMON_BACKUP_DIR', async ({ page }) => {
	const dir = process.env['SMON_E2E_BACKUP_DIR'] ?? '';
	await page.goto('/');
	// Two clicks like delete: the first arms, the second does the disk work.
	await page.getByRole('button', { name: 'Back up now' }).click();
	await page.getByRole('button', { name: 'Confirm backup' }).click();
	await expect(page.getByText(/saved monitor-\S+\.db \(/)).toBeVisible({
		timeout: 15_000
	});
	const names = fs.readdirSync(dir).filter((n) => /^monitor-.*\.db$/.test(n));
	expect(names.length, `no backup file in ${dir}`).toBeGreaterThan(0);
});

test('keyboard navigation reaches the detail page', async ({ page, request }) => {
	const name = `e2e-kbd-${Date.now()}`;
	const id = await addMonitor(request, name, `http://127.0.0.1:${FAULT}/ok`);
	await page.goto('/');

	const link = page.getByRole('link', { name });
	await link.focus();
	await expect(link).toBeFocused();
	await page.keyboard.press('Enter');
	await expect(page).toHaveURL(new RegExp(`/monitors/${id}`));
	await expect(page.getByRole('heading', { name })).toBeVisible();
});

test('sign-in: wrong token refused, right token earns the session, sign-out ends it', async ({
	page,
	context
}) => {
	// The shared beforeEach already logged this context in — start truly signed out.
	await context.clearCookies();
	await page.goto('/');
	// A dead session is a redirect to the sign-in card — never a half-board.
	await expect(page).toHaveURL(/\/login\?return=/);
	await expect(page.getByTestId('auth-card')).toBeVisible();

	// The operator-token path folds under technical details.
	await page.getByText('Technical details').click();

	// A wrong token is refused with the server's own words, no cookie.
	await page.getByTestId('token-input').fill('wrong-token');
	await page.getByTestId('token-submit').click();
	await expect(page.getByTestId('auth-error')).toHaveText(/invalid token/);

	// The right token → 204 + cookie → reload → the real board, not a stored token.
	await page.getByTestId('token-input').fill(TOKEN);
	await page.getByTestId('token-submit').click();
	await expect(page.getByRole('heading', { name: 'Monitors' })).toBeVisible({
		timeout: 10_000
	});
	// The session cookie is HttpOnly — document.cookie can never see it.
	const cookies = await context.cookies();
	const session = cookies.find((c) => c.name === '__Host-smon_session');
	expect(session, 'the login sets the host-only session cookie').toBeTruthy();
	expect(session?.httpOnly).toBe(true);
	expect(session?.sameSite).toBe('Strict');
	expect(session?.secure).toBe(true);
	// A __Host- cookie must carry no Domain attribute. The browser's jar reports the
	// host for a host-only cookie, so the claim is checked on the header itself.
	const header = (await page.request.post('/api/login', { data: { token: TOKEN } })).headers()[
		'set-cookie'
	];
	expect(header).toMatch(/^__Host-smon_session=/);
	expect(header, 'a __Host- cookie carries no Domain').not.toMatch(/;\s*domain=/i);
	expect(header).toMatch(/;\s*path=\/(;|$)/i);

	// Sign out deletes the row server-side and expires the cookie — the card returns.
	await page.getByTestId('account-menu').click();
	await page.getByTestId('sign-out').click();
	await expect(page).toHaveURL(/\/login/, { timeout: 10_000 });
	const after = await context.cookies();
	expect(after.find((c) => c.name === '__Host-smon_session')?.value ?? '').toBe('');
});
