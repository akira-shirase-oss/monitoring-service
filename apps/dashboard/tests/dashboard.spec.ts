import { expect, test, type APIRequestContext, type Page } from '@playwright/test';

const FAULT = process.env['SMON_E2E_FAULT_PORT'] ?? '58710';

async function addMonitor(
	request: APIRequestContext,
	name: string,
	url: string
): Promise<string> {
	const response = await request.post('/api/monitors', {
		data: { name, url, interval_s: 15, timeout_s: 5 }
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
	await expect(page.getByText('Incidents')).toBeVisible();
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

	// Edit: the form seeds current values; every management write bumps generation
	// once — create(1) → pause(2) → edit(3).
	await page.getByRole('button', { name: 'Edit' }).click();
	await page.getByPlaceholder('60').fill('30');
	await page.getByRole('button', { name: 'Save changes' }).click();
	await expect(page.getByText(/every 30s/)).toBeVisible();
	await expect(page.getByText(/generation 3/)).toBeVisible();

	// Delete needs the two-click confirm; a paused, checkless monitor deletes clean.
	await page.getByRole('button', { name: 'Delete' }).click();
	await page.getByRole('button', { name: 'Confirm delete' }).click();
	await expect(page).toHaveURL(/\/$/);
	await expect(page.getByRole('link', { name })).not.toBeVisible();
});

test('delete refusal on a monitor with history shows the server error', async ({
	page,
	request
}) => {
	const name = `e2e-del-${Date.now()}`;
	const id = await addMonitor(request, name, `http://127.0.0.1:${FAULT}/ok`);
	// Wait for committed evidence — delete must then refuse (evidence isn't erasable).
	await expect
		.poll(
			async () => {
				const r = await request.get(`/api/monitors/${id}/checks?limit=1`);
				return (await r.json()).checks.length;
			},
			{ timeout: 40_000 }
		)
		.toBeGreaterThan(0);

	await page.goto(`/monitors/${id}`);
	await page.getByRole('button', { name: 'Delete' }).click();
	await page.getByRole('button', { name: 'Confirm delete' }).click();
	// The server's own conflict message is shown — not a silent no-op, not a spin.
	await expect(page.getByText(/recorded history|pause it instead/)).toBeVisible();
	await expect(page.getByRole('heading', { name })).toBeVisible();
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
	await request.patch(`/api/monitors/${id}`, { data: { paused: true } });
	await new Promise((resolve) => setTimeout(resolve, 35_000));
	await request.patch(`/api/monitors/${id}`, { data: { paused: false } });
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
	await expect(page.getByText('The server rejected the token')).toBeVisible();
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
		}
	});
	expect(response.status()).toBe(201);

	await page.goto(`/monitors/${id}`);
	await expect(page.getByText('Maintenance windows')).toBeVisible();
	await expect(page.getByText('suppressing now')).toBeVisible();
	await expect(page.getByText('e2e deploy rehearsal')).toBeVisible();
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
