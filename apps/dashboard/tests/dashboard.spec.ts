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
