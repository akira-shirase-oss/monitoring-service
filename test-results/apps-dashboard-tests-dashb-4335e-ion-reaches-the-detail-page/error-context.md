# Instructions

- Following Playwright test failed.
- Explain why, be concise, respect Playwright best practices.
- Provide a snippet of code with the fix, if possible.

# Test info

- Name: apps/dashboard/tests/dashboard.spec.ts >> keyboard navigation reaches the detail page
- Location: apps/dashboard/tests/dashboard.spec.ts:79:1

# Error details

```
TypeError: apiRequestContext.post: Invalid URL
```

# Test source

```ts
  1  | import { expect, test, type APIRequestContext, type Page } from '@playwright/test';
  2  | 
  3  | const FAULT = process.env['SMON_E2E_FAULT_PORT'] ?? '58710';
  4  | 
  5  | async function addMonitor(
  6  | 	request: APIRequestContext,
  7  | 	name: string,
  8  | 	url: string
  9  | ): Promise<string> {
> 10 | 	const response = await request.post('/api/monitors', {
     |                                 ^ TypeError: apiRequestContext.post: Invalid URL
  11 | 		data: { name, url, interval_s: 15, timeout_s: 5 }
  12 | 	});
  13 | 	expect(response.status()).toBe(201);
  14 | 	return (await response.json()).id as string;
  15 | }
  16 | 
  17 | test('an unreachable API shows "cannot see", never a green board', async ({ page }) => {
  18 | 	// The dashboard HTML loads (it is a static asset); the API answering it fails.
  19 | 	await page.route('**/api/**', (route) => route.abort());
  20 | 	await page.goto('/');
  21 | 	await expect(page.getByText('The monitor server is unreachable.')).toBeVisible();
  22 | 	// Nothing fabricated: no monitor rows, no statuses.
  23 | 	await expect(page.getByText('No monitors yet.')).not.toBeVisible();
  24 | });
  25 | 
  26 | test('a monitor registers, probes land, and the badge turns up', async ({
  27 | 	page,
  28 | 	request
  29 | }) => {
  30 | 	const name = `e2e-up-${Date.now()}`;
  31 | 	await addMonitor(request, name, `http://127.0.0.1:${FAULT}/ok`);
  32 | 	await page.goto('/');
  33 | 	await expect(page.getByRole('link', { name })).toBeVisible();
  34 | 
  35 | 	// First probe lands on the next 15s boundary; status starts `unknown` and must not
  36 | 	// be painted up before a check commits. Poll the API — the badge follows evidence.
  37 | 	await expect
  38 | 		.poll(async () => {
  39 | 			const response = await request.get('/api/monitors');
  40 | 			const monitors = (await response.json()).monitors;
  41 | 			return monitors.find((m: { name: string }) => m.name === name)?.status;
  42 | 		}, { timeout: 40_000 })
  43 | 		.toBe('up');
  44 | 	await page.reload();
  45 | 	await expect(page.getByRole('link', { name })).toBeVisible();
  46 | });
  47 | 
  48 | test('three failures open an incident and the detail page tells the truth', async ({
  49 | 	page,
  50 | 	request
  51 | }) => {
  52 | 	const name = `e2e-down-${Date.now()}`;
  53 | 	const id = await addMonitor(request, name, `http://127.0.0.1:${FAULT}/fail`);
  54 | 
  55 | 	// 3 consecutive HTTP-500 probes at a 15s interval → incident in ~45s. Wait on the
  56 | 	// committed incident, not on a timer.
  57 | 	await expect
  58 | 		.poll(
  59 | 			async () => {
  60 | 				const response = await request.get(`/api/incidents?monitor_id=${id}&state=open`);
  61 | 				return (await response.json()).incidents.length;
  62 | 			},
  63 | 			{ timeout: 75_000 }
  64 | 		)
  65 | 		.toBe(1);
  66 | 
  67 | 	await page.goto(`/monitors/${id}`);
  68 | 	// Detail page: down badge, evidence strip with http_fail rows, the open incident.
  69 | 	await expect(page.getByRole('heading', { name })).toBeVisible();
  70 | 	await expect(page.getByText('down').first()).toBeVisible();
  71 | 	await expect(page.getByText('Recent checks')).toBeVisible();
  72 | 	await expect(page.getByText('HTTP 500').first()).toBeVisible();
  73 | 	await expect(page.getByText('Incidents')).toBeVisible();
  74 | 	await expect(page.getByText('still open').first()).toBeVisible();
  75 | 	// Coverage vs success are separate claims — success reads 0%, coverage is a number.
  76 | 	await expect(page.getByText(/coverage \d+\.\d%[\s\S]*success 0\.0%/)).toBeVisible();
  77 | });
  78 | 
  79 | test('keyboard navigation reaches the detail page', async ({ page, request }) => {
  80 | 	const name = `e2e-kbd-${Date.now()}`;
  81 | 	const id = await addMonitor(request, name, `http://127.0.0.1:${FAULT}/ok`);
  82 | 	await page.goto('/');
  83 | 
  84 | 	const link = page.getByRole('link', { name });
  85 | 	await link.focus();
  86 | 	await expect(link).toBeFocused();
  87 | 	await page.keyboard.press('Enter');
  88 | 	await expect(page).toHaveURL(new RegExp(`/monitors/${id}`));
  89 | 	await expect(page.getByRole('heading', { name })).toBeVisible();
  90 | });
  91 | 
```