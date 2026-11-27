import { expect, test, type APIRequestContext, type BrowserContext, type Page } from '@playwright/test';

// The hosted journeys: an account, its workspace, and everything the workspace pages do.
// Each test signs up its own account, so they don't share a roster, a webhook or a
// status page — only the server, the fixture and the clock.

const FAULT = process.env['SMON_E2E_FAULT_PORT'] ?? '58710';
const TOKEN = 'e2e-token-for-the-playwright-suite-0001';
// The operator's bearer: mints invite codes and lifts a workspace off the free plan.
const OPERATOR = { authorization: `Bearer ${TOKEN}` };
const PASSWORD = 'e2e-passphrase-for-the-suite';

let serial = 0;
function address(tag: string): string {
	serial += 1;
	return `${tag}-${Date.now()}-${serial}@example.com`;
}

async function inviteCode(request: APIRequestContext): Promise<string> {
	const response = await request.post('/api/invites', { data: {}, headers: OPERATOR });
	expect(response.status()).toBe(201);
	return (await response.json()).invite.code as string;
}

/** The free plan's floor is one check a minute; the suite can't wait that long. */
async function liftPlan(request: APIRequestContext, workspaceId: string): Promise<void> {
	const response = await request.patch(`/api/workspaces/${workspaceId}/plan`, {
		data: { plan: 'operator' },
		headers: OPERATOR
	});
	expect(response.status()).toBe(200);
}

/** A signed-in account whose session lives in `context` — page and API calls alike. */
async function account(
	context: BrowserContext,
	request: APIRequestContext,
	origin: string,
	tag: string,
	plan: 'free' | 'operator' = 'operator'
): Promise<{ email: string; workspaceId: string }> {
	const email = address(tag);
	const signup = await context.request.post('/api/signup', {
		data: { email, password: PASSWORD, invite: await inviteCode(request) },
		headers: { origin }
	});
	expect(signup.status()).toBe(201);
	const workspaceId = (await signup.json()).account.workspace as string;
	const login = await context.request.post('/api/login', {
		data: { email, password: PASSWORD },
		headers: { origin }
	});
	expect(login.status()).toBe(204);
	if (plan === 'operator') await liftPlan(request, workspaceId);
	return { email, workspaceId };
}

/** A cookie-authed write the way the dashboard sends it: same-origin, JSON. */
function sameOrigin(origin: string) {
	return { origin, 'content-type': 'application/json' };
}

async function fillMonitor(page: Page, name: string, url: string): Promise<void> {
	await page.getByPlaceholder('billing-api').fill(name);
	await page.getByPlaceholder('https://example.com/healthz').fill(url);
	await page.getByPlaceholder('60').fill('15');
}

test('a new account goes from sign-up to a public status page', async ({
	page,
	request,
	browser,
	baseURL
}) => {
	const origin = baseURL ?? '';
	const email = address('journey');
	const code = await inviteCode(request);

	// Sign-up lands on the guide, not on an empty board.
	await page.goto('/signup');
	await page.locator('#auth-email').fill(email);
	await page.locator('#auth-password').fill(PASSWORD);
	await page.locator('#auth-invite').fill(code);
	await page.getByTestId('password-submit').click();
	await expect(page).toHaveURL(/\/onboarding$/, { timeout: 15_000 });
	await expect(page.getByRole('heading', { name: 'Get started' })).toBeVisible();
	for (const step of ['step-name', 'step-monitor', 'step-alerts', 'step-checks']) {
		await expect(page.getByTestId(step)).toHaveAttribute('data-done', 'false');
	}

	const workspaces = await page.request.get('/api/workspaces');
	const workspaceId = (await workspaces.json()).workspaces[0].id as string;
	await liftPlan(request, workspaceId);

	// 1. The name: the step ticks because the workspace was renamed, and the shell shows it.
	const workspaceName = `Journey ${Date.now()}`;
	await page.getByTestId('guide-name').fill(workspaceName);
	await page.getByTestId('guide-name-save').click();
	await expect(page.getByTestId('step-name')).toHaveAttribute('data-done', 'true');
	await expect(page.getByTestId('workspace-name')).toHaveText(workspaceName);

	// 2. The first URL.
	const monitorName = `journey-api-${Date.now()}`;
	await fillMonitor(page, monitorName, `http://127.0.0.1:${FAULT}/ok`);
	await page.getByRole('button', { name: 'Start watching' }).click();
	await expect(page.getByTestId('step-monitor')).toHaveAttribute('data-done', 'true');

	// 3. The alert channel, proven with a test alert the receiver really got — signed.
	await request.get(`http://127.0.0.1:${FAULT}/reset`);
	await page.getByTestId('guide-webhook').fill(`http://127.0.0.1:${FAULT}/sink`);
	await page.getByTestId('guide-webhook-save').click();
	await expect(page.getByTestId('step-alerts')).toHaveAttribute('data-done', 'true');
	await page.getByTestId('guide-test').click();
	await expect(page.getByTestId('guide-test-result')).toContainText('Delivered');
	const sink = await request.get(`http://127.0.0.1:${FAULT}/sink/events`);
	const events = (await sink.json()).events as { signature: string | null; body: string }[];
	expect(events.length).toBe(1);
	expect(events[0]?.signature).toMatch(/^sha256=[0-9a-f]{64}$/);
	expect(JSON.parse(events[0]?.body ?? '{}').kind).toBe('test');

	// 4. The first committed check ticks the last step by itself.
	await expect(page.getByTestId('step-checks')).toHaveAttribute('data-done', 'true', {
		timeout: 45_000
	});

	// Finishing puts the guide away and opens the board.
	await page.getByTestId('guide-finish').click();
	await expect(page.getByRole('heading', { name: 'Monitors' })).toBeVisible();
	await expect(page.getByTestId('nav-guide')).toHaveCount(0);

	// The status page: off until it has a title, then public with only the chosen monitor.
	await page.goto('/settings/status-page');
	await expect(page.getByTestId('status-page-state')).toHaveText('Off');
	const slug = `journey-${Date.now()}`;
	await page.getByTestId('status-title').fill('Journey status');
	await page.getByTestId('status-slug').fill(slug);
	await page.getByLabel(monitorName).check();
	await page.getByTestId('status-save').click();
	await expect(page.getByTestId('status-saved')).toBeVisible();
	await expect(page.getByTestId('status-page-state')).toHaveText('Public');

	// A visitor with no session at all sees the page — the name, never the URL.
	const visitor = await browser.newContext();
	const publicPage = await visitor.newPage();
	await publicPage.goto(`${origin}/s/${slug}`);
	await expect(publicPage.getByTestId('public-title')).toHaveText('Journey status');
	await expect(publicPage.getByTestId('public-service')).toHaveCount(1, { timeout: 45_000 });
	await expect(publicPage.getByTestId('public-service')).toContainText(monitorName);
	await expect(publicPage.locator('body')).not.toContainText(`127.0.0.1:${FAULT}`);

	// An address nobody published is said plainly, not rendered as an empty healthy page.
	await publicPage.goto(`${origin}/s/nobody-published-this`);
	await expect(publicPage.getByTestId('public-absent')).toBeVisible();
	await visitor.close();
});

test('an API token is shown once, works, and stops working when revoked', async ({
	page,
	context,
	request,
	baseURL
}) => {
	await account(context, request, baseURL ?? '', 'tokens');
	await page.goto('/settings/tokens');
	await expect(page.getByTestId('empty-tokens')).toBeVisible();

	await page.getByTestId('new-token').click();
	await page.getByTestId('token-name').fill('deploy script');
	await page.getByTestId('token-create').click();

	const secret = await page.getByTestId('token-secret').inputValue();
	expect(secret).toMatch(/^smon_/);
	const works = await request.get('/api/monitors', {
		headers: { authorization: `Bearer ${secret}` }
	});
	expect(works.status()).toBe(200);
	// The default is a viewer token: it reads, and a write is refused.
	const write = await request.post('/api/monitors', {
		data: { name: 'not-allowed', url: `http://127.0.0.1:${FAULT}/ok` },
		headers: { authorization: `Bearer ${secret}` }
	});
	expect(write.status()).toBe(403);

	// Once the dialog closes, the secret is nowhere on the page.
	await page.getByTestId('token-secret-done').click();
	await expect(page.getByTestId('token-secret-dialog')).toHaveCount(0);
	await expect(page.locator('body')).not.toContainText(secret);
	await expect(page.getByTestId('token-row')).toContainText('deploy script');

	await page.getByTestId('token-revoke').click();
	await page.getByTestId('confirm-action').click();
	await expect(page.getByTestId('token-row')).toContainText('Revoked');
	const after = await request.get('/api/monitors', {
		headers: { authorization: `Bearer ${secret}` }
	});
	expect(after.status()).toBe(401);
});

test('an invited person sees the invitation, joins, and a viewer can only read', async ({
	page,
	context,
	request,
	browser,
	baseURL
}) => {
	const origin = baseURL ?? '';
	const owner = await account(context, request, origin, 'owner');

	// The invitee already has an account of their own.
	const guestContext = await browser.newContext();
	const guest = await account(guestContext, request, origin, 'guest');

	await page.goto('/settings/members');
	await expect(page.getByTestId('member-row')).toHaveCount(1);
	await page.getByTestId('invite-email').fill(guest.email);
	await page.getByTestId('invite-send').click();
	await expect(page.getByTestId('invite-note')).toContainText(guest.email);
	await expect(page.getByTestId('sent-invitations')).toContainText(guest.email);
	// An invitation is not a seat: the roster is unchanged until it is accepted.
	await expect(page.getByTestId('member-row')).toHaveCount(1);

	// The guest is told on whatever page they open, then joins from the Members page.
	const guestPage = await guestContext.newPage();
	await guestPage.goto(`${origin}/`);
	await expect(guestPage.getByTestId('invited-banner')).toBeVisible();
	await guestPage.getByTestId('invited-banner').click();
	await expect(guestPage.getByTestId('received-invitations')).toBeVisible();
	await guestPage.getByTestId('invitation-accept').click();
	await expect(guestPage.getByTestId('received-invitations')).toHaveCount(0);
	await expect(guestPage.getByTestId('invited-banner')).toHaveCount(0);

	// The owner's roster now holds both, and the pending list is empty.
	await page.reload();
	await expect(page.getByTestId('member-row')).toHaveCount(2);
	await expect(page.getByTestId('sent-invitations')).toHaveCount(0);

	// In the owner's workspace the guest is a viewer: reads answer, writes are refused.
	const inOwners = { ...sameOrigin(origin), 'x-smon-workspace': owner.workspaceId };
	const read = await guestContext.request.get('/api/monitors', { headers: inOwners });
	expect(read.status()).toBe(200);
	const write = await guestContext.request.post('/api/monitors', {
		data: { name: 'viewer-write', url: `http://127.0.0.1:${FAULT}/ok` },
		headers: inOwners
	});
	expect(write.status()).toBe(403);

	// The viewer's own view of that workspace offers no invite form and no role controls.
	await guestPage.getByTestId('workspace-switcher').click();
	await guestPage.getByTestId('workspace-option').filter({ hasText: 'viewer' }).click();
	await guestPage.goto(`${origin}/settings/members`);
	await expect(guestPage.getByTestId('member-row')).toHaveCount(2);
	await expect(guestPage.getByTestId('invite-email')).toHaveCount(0);
	await expect(guestPage.getByTestId('member-remove')).toHaveCount(0);

	// The owner removes the guest; the roster is back to one.
	await page.getByTestId('member-row').filter({ hasText: guest.email }).getByTestId('member-remove').click();
	await page.getByTestId('confirm-action').click();
	await expect(page.getByTestId('member-row')).toHaveCount(1);
	await guestContext.close();
});

test('the alerts page connects, tests and removes a webhook', async ({
	page,
	context,
	request,
	baseURL
}) => {
	await account(context, request, baseURL ?? '', 'alerts');
	await page.goto('/settings/alerts');
	await expect(page.getByTestId('webhook-state')).toHaveText('No alert channel connected');

	// Not a URL the server will take: its own words are shown, and nothing is saved.
	await page.getByTestId('webhook-url').fill('ftp://example.com/hook');
	await page.getByTestId('webhook-save').click();
	await expect(page.getByTestId('webhook-error')).toBeVisible();
	await expect(page.getByTestId('webhook-state')).toHaveText('No alert channel connected');

	await page.getByTestId('webhook-url').fill(`http://127.0.0.1:${FAULT}/sink`);
	await page.getByTestId('webhook-save').click();
	await expect(page.getByTestId('webhook-state')).toHaveText('Connected');
	// The secret is there to copy but masked until asked for.
	await expect(page.getByTestId('webhook-secret')).toHaveAttribute('type', 'password');

	await page.getByTestId('webhook-test').click();
	await expect(page.getByTestId('webhook-test-result')).toContainText('Delivered');

	await page.getByTestId('webhook-remove').click();
	await page.getByTestId('confirm-action').click();
	await expect(page.getByTestId('webhook-state')).toHaveText('No alert channel connected');
	await expect(page.getByTestId('webhook-secret')).toHaveCount(0);
});

test('incidents list what opened, and a maintenance window schedules and cancels', async ({
	page,
	context,
	request,
	baseURL
}) => {
	const origin = baseURL ?? '';
	await account(context, request, origin, 'incidents');

	await page.goto('/incidents');
	await expect(page.getByTestId('empty-incidents')).toBeVisible();
	await page.goto('/maintenance');
	await expect(page.getByTestId('empty-maintenance')).toContainText('Add a monitor');

	const name = `failing-${Date.now()}`;
	const created = await context.request.post('/api/monitors', {
		data: { name, url: `http://127.0.0.1:${FAULT}/fail`, interval_s: 15, timeout_s: 5 },
		headers: sameOrigin(origin)
	});
	expect(created.status()).toBe(201);

	// A window for tomorrow: listed with its monitor and reason, then cancelled.
	await page.goto('/maintenance');
	await page.getByTestId('new-window').click();
	const day = new Date(Date.now() + 86_400_000);
	const stamp = (hour: number) =>
		`${day.getFullYear()}-${String(day.getMonth() + 1).padStart(2, '0')}-${String(day.getDate()).padStart(2, '0')}T${String(hour).padStart(2, '0')}:00`;
	await page.getByTestId('window-starts').fill(stamp(10));
	await page.getByTestId('window-ends').fill(stamp(11));
	await page.getByTestId('window-reason').fill('database upgrade');
	await page.getByTestId('window-create').click();
	await expect(page.getByTestId('window-row')).toHaveCount(1);
	await expect(page.getByTestId('window-row')).toContainText(name);
	await expect(page.getByTestId('window-row')).toContainText('database upgrade');
	await page.getByTestId('window-cancel').click();
	await page.getByTestId('confirm-action').click();
	await expect(page.getByTestId('window-row')).toHaveCount(0);

	// Three failures at 15 s open the incident; the page picks it up on its own poll.
	await page.goto('/incidents');
	const open = page.getByTestId('incident-row').filter({ hasText: name });
	await expect(open).toHaveAttribute('data-state', 'open', { timeout: 90_000 });
	await expect(page.getByTestId('open-count')).toHaveText('1');
});

test('a monitor keeps its request method and expectations, and HEAD drops the body text', async ({
	page,
	context,
	request,
	baseURL
}) => {
	await account(context, request, baseURL ?? '', 'expect');
	await page.goto('/');
	await page.getByTestId('empty-monitors').getByRole('button').click();
	const name = `expect-${Date.now()}`;
	await fillMonitor(page, name, `http://127.0.0.1:${FAULT}/ok`);
	await page.getByTestId('monitor-statuses').fill('200-204, 301');
	await page.getByTestId('monitor-body').fill('ok');
	await page.getByRole('button', { name: 'Create monitor' }).click();

	await page.getByRole('link', { name }).click();
	const line = page.getByTestId('monitor-expectations');
	await expect(line).toContainText('GET request');
	await expect(line).toContainText('200-204,301');
	await expect(line).toContainText('body must contain “ok”');

	// The API holds exactly what the form sent.
	const listed = await context.request.get('/api/monitors');
	const stored = ((await listed.json()).monitors as { name: string; method: string; expect_body: string | null }[]).find(
		(m) => m.name === name
	);
	expect(stored?.method).toBe('GET');
	expect(stored?.expect_body).toBe('ok');

	// HEAD has no body to search: the field locks, and saving removes the stored text.
	await page.getByRole('button', { name: 'Edit' }).click();
	await page.getByTestId('monitor-method').selectOption('HEAD');
	await expect(page.getByTestId('monitor-body')).toBeDisabled();
	await page.getByRole('button', { name: 'Save changes' }).click();
	await expect(line).toContainText('HEAD request');
	await expect(line).not.toContainText('body must contain');
});

test('the free plan shows its limits and names the one a write hits', async ({
	page,
	context,
	request,
	baseURL
}) => {
	await account(context, request, baseURL ?? '', 'plan', 'free');
	await page.goto('/settings/general');
	await expect(page.getByTestId('plan-name')).toHaveText('free');
	await expect(page.getByTestId('plan-limits')).toContainText('0 of 10 in use');
	await expect(page.getByTestId('plan-limits')).toContainText('1 min');
	await expect(page.getByTestId('workspace-role')).toContainText('Owner');

	// Renaming here is the same write the guide does; the shell follows.
	await page.getByTestId('workspace-rename').fill('Plan check');
	await page.getByTestId('workspace-rename-save').click();
	await expect(page.getByTestId('workspace-name')).toHaveText('Plan check');

	// A 15-second interval is below the plan's floor: the server's sentence is shown.
	await page.goto('/');
	await page.getByTestId('empty-monitors').getByRole('button').click();
	await fillMonitor(page, 'too-fast', `http://127.0.0.1:${FAULT}/ok`);
	await page.getByRole('button', { name: 'Create monitor' }).click();
	await expect(page.getByRole('alert')).toContainText('below the 60s minimum');
});
