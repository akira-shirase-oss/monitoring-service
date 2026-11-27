// The read-mostly pages, rendered with the data their loads would hand them. Each test
// states one thing the page must tell the truth about.
import { fireEvent, render, screen, within } from '@testing-library/svelte';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { pick, workspace } from '#lib/workspace.svelte.js';
import IncidentsPage from '../routes/(app)/incidents/+page.svelte';
import MaintenancePage from '../routes/(app)/maintenance/+page.svelte';
import AccountPage from '../routes/(app)/account/+page.svelte';
import AlertsPage from '../routes/(app)/settings/alerts/+page.svelte';
import GeneralPage from '../routes/(app)/settings/general/+page.svelte';
import PublicPage from '../routes/s/[slug]/+page.svelte';

vi.mock('$app/navigation', () => ({ invalidateAll: vi.fn(async () => {}), goto: vi.fn(async () => {}) }));

const FREE = { max_monitors: 10, min_interval_s: 60, check_retention_days: 2, incident_retention_days: 90 };

function signInAs(role: string, plan = 'free', limits: typeof FREE | null = FREE) {
	workspace.list = [{ id: 'ws-1', slug: 'acme', name: 'Acme', role, plan, limits }];
	workspace.home = 'ws-1';
	pick('');
}

const data = <T>(own: T) => ({ data: own }) as never;

beforeEach(() => signInAs('owner'));

describe('Incidents', () => {
	const open = {
		id: 2,
		monitor_id: 'm1',
		opened_at: '2026-11-27T10:00:45Z',
		first_fail_at: '2026-11-27T10:00:00Z',
		last_fail_at: '2026-11-27T10:05:00Z',
		resolved_at: null,
		state: 'open'
	};
	const closed = {
		id: 1,
		monitor_id: 'm-gone',
		opened_at: '2026-11-26T09:00:45Z',
		first_fail_at: '2026-11-26T09:00:00Z',
		last_fail_at: '2026-11-26T09:10:00Z',
		resolved_at: '2026-11-26T09:30:00Z',
		state: 'resolved'
	};

	it('separates what is open from what recovered and counts the open ones', () => {
		render(IncidentsPage, { props: data({ incidents: [open, closed], names: { m1: 'api' }, failure: null }) });
		const rows = screen.getAllByTestId('incident-row');
		expect(rows.map((row) => row.getAttribute('data-state'))).toEqual(['open', 'resolved']);
		expect(screen.getByTestId('open-count').textContent).toBe('1');
		expect(rows[0]?.textContent).toContain('api');
	});

	it('measures a recovered incident from its first failure, not from when it opened', () => {
		render(IncidentsPage, { props: data({ incidents: [closed], names: {}, failure: null }) });
		// 09:00:00 to 09:30:00 — the 45 seconds before it opened are part of the outage.
		expect(screen.getByTestId('incident-row').textContent).toContain('30 min');
	});

	it('keeps the incident of a monitor that no longer exists, and says so', () => {
		render(IncidentsPage, { props: data({ incidents: [closed], names: {}, failure: null }) });
		expect(screen.getByTestId('incident-row').textContent).toContain('Deleted monitor');
	});

	it('does not call an empty list healthy', () => {
		render(IncidentsPage, { props: data({ incidents: [], names: {}, failure: null }) });
		const empty = screen.getByTestId('empty-incidents');
		expect(empty.textContent).toContain('No incidents recorded');
		expect(empty.textContent).toContain('not a promise that everything is healthy');
	});

	it('shows a failed read as a failure, never as "no incidents"', () => {
		render(IncidentsPage, {
			props: data({ incidents: [], names: {}, failure: { status: 0, message: 'unreachable' } })
		});
		expect(screen.getByTestId('problem')).toBeTruthy();
		expect(screen.queryByTestId('empty-incidents')).toBeNull();
	});
});

describe('Maintenance', () => {
	const monitors = [{ id: 'm1', name: 'api' }];
	const future = {
		id: 7,
		monitor_id: 'm1',
		starts_at: '2099-01-01T10:00:00Z',
		ends_at: '2099-01-01T11:00:00Z',
		reason: 'database upgrade',
		active: false,
		created_at: '2026-11-27T10:00:00Z'
	};
	const past = { ...future, id: 3, starts_at: '2020-01-01T10:00:00Z', ends_at: '2020-01-01T11:00:00Z', reason: null };

	it('lists upcoming windows with a way to cancel, and finished ones without', () => {
		render(MaintenancePage, { props: data({ monitors, windows: [past, future], unread: [], failure: null }) });
		const rows = screen.getAllByTestId('window-row');
		expect(rows).toHaveLength(2);
		expect(within(rows[0] as HTMLElement).getByTestId('window-cancel')).toBeTruthy();
		expect(rows[0]?.textContent).toContain('database upgrade');
		expect(within(rows[1] as HTMLElement).queryByTestId('window-cancel')).toBeNull();
	});

	it('gives a viewer the list and nothing to change', () => {
		signInAs('viewer');
		render(MaintenancePage, { props: data({ monitors, windows: [future], unread: [], failure: null }) });
		expect(screen.getByTestId('window-row')).toBeTruthy();
		expect(screen.queryByTestId('new-window')).toBeNull();
		expect(screen.queryByTestId('window-cancel')).toBeNull();
	});

	it('names the monitors whose windows could not be read, instead of looking complete', () => {
		render(MaintenancePage, { props: data({ monitors, windows: [], unread: ['billing'], failure: null }) });
		expect(screen.getByRole('alert').textContent).toContain('billing');
		expect(screen.getByRole('alert').textContent).toContain('incomplete');
	});

	it('asks for a monitor first when there is none to schedule for', () => {
		render(MaintenancePage, { props: data({ monitors: [], windows: [], unread: [], failure: null }) });
		expect(screen.getByTestId('empty-maintenance').textContent).toContain('Add a monitor');
		expect(screen.queryByTestId('new-window')).toBeNull();
	});
});

describe('Alerts', () => {
	const webhook = { url: 'https://hooks.example.com/x', secret: 'whsec_do_not_show' };

	it('keeps the signing secret masked until asked', async () => {
		render(AlertsPage, { props: data({ webhook, failure: null }) });
		const field = screen.getByTestId('webhook-secret') as HTMLInputElement;
		expect(field.type).toBe('password');
		await fireEvent.click(screen.getByRole('button', { name: 'Show' }));
		expect((screen.getByTestId('webhook-secret') as HTMLInputElement).type).toBe('text');
	});

	it('says plainly when no channel is connected, and offers no test send', () => {
		render(AlertsPage, { props: data({ webhook: null, failure: null }) });
		expect(screen.getByTestId('webhook-state').textContent).toBe('No alert channel connected');
		expect(screen.queryByTestId('webhook-test')).toBeNull();
		expect(screen.queryByTestId('webhook-secret')).toBeNull();
	});

	it('gives a viewer no save, no test send and no removal', () => {
		signInAs('viewer');
		render(AlertsPage, { props: data({ webhook, failure: null }) });
		expect(screen.queryByTestId('webhook-save')).toBeNull();
		expect(screen.queryByTestId('webhook-test')).toBeNull();
		expect(screen.queryByTestId('webhook-remove')).toBeNull();
		expect((screen.getByTestId('webhook-url') as HTMLInputElement).readOnly).toBe(true);
	});

	it("shows a test alert's result with the server's word beside the sentence", async () => {
		vi.stubGlobal(
			'fetch',
			vi.fn(async () => new Response(JSON.stringify({ test: { ok: false, detail: 'http_500' } }), { status: 200 }))
		);
		render(AlertsPage, { props: data({ webhook, failure: null }) });
		await fireEvent.click(screen.getByTestId('webhook-test'));
		const result = await screen.findByTestId('webhook-test-result');
		expect(result.textContent).toContain('Not delivered');
		expect(result.textContent).toContain('HTTP 500');
		expect(result.textContent).toContain('http_500');
	});

	it("shows the server's refusal of a URL and does not claim it saved", async () => {
		vi.stubGlobal(
			'fetch',
			vi.fn(
				async () =>
					new Response(JSON.stringify({ error: { code: 'refused_target', message: 'that address is not allowed' } }), {
						status: 422
					})
			)
		);
		render(AlertsPage, { props: data({ webhook: null, failure: null }) });
		await fireEvent.input(screen.getByTestId('webhook-url'), { target: { value: 'http://10.0.0.1/hook' } });
		await fireEvent.click(screen.getByTestId('webhook-save'));
		expect((await screen.findByTestId('webhook-error')).textContent).toContain('that address is not allowed');
		expect(screen.queryByText('Saved.')).toBeNull();
	});
});

describe('General', () => {
	it("shows the free plan's limits beside what is in use", () => {
		render(GeneralPage, { props: data({ monitorCount: 3 }) });
		expect(screen.getByTestId('plan-name').textContent).toBe('free');
		const limits = screen.getByTestId('plan-limits').textContent ?? '';
		expect(limits).toContain('3 of 10 in use');
		expect(limits).toContain('1 min');
		expect(limits).toContain('2 days');
		expect(limits).toContain('90 days');
	});

	it('says there is no plan limit rather than inventing a number', () => {
		signInAs('owner', 'operator', {
			max_monitors: null,
			min_interval_s: 15,
			check_retention_days: null,
			incident_retention_days: null
		} as never);
		render(GeneralPage, { props: data({ monitorCount: 42 }) });
		const limits = screen.getByTestId('plan-limits').textContent ?? '';
		expect(limits).toContain('42 in use · no plan limit');
		expect(limits).toContain("the server's setting");
	});

	it('lets a viewer read the name but not change it', () => {
		signInAs('viewer');
		render(GeneralPage, { props: data({ monitorCount: 0 }) });
		expect((screen.getByTestId('workspace-rename') as HTMLInputElement).readOnly).toBe(true);
		expect(screen.queryByTestId('workspace-rename-save')).toBeNull();
		expect(screen.getByTestId('workspace-role').textContent).toContain('Viewer');
	});

	it('offers the export to an owner and to the operator, and to nobody else', () => {
		for (const [role, offered] of [
			['owner', true],
			['operator', true],
			['member', false],
			['viewer', false]
		] as const) {
			signInAs(role);
			const page = render(GeneralPage, { props: data({ monitorCount: 0 }) });
			expect(screen.queryByTestId('export-data') !== null, role).toBe(offered);
			page.unmount();
		}
	});

	it("shows the server's refusal of an export and starts no download", async () => {
		const made = vi.spyOn(URL, 'createObjectURL');
		vi.stubGlobal(
			'fetch',
			vi.fn(async () =>
				Response.json(
					{ error: { code: 'too_many_requests', message: 'one export every 10 seconds per workspace; retry shortly' } },
					{ status: 429 }
				)
			)
		);
		render(GeneralPage, { props: data({ monitorCount: 0 }) });
		await fireEvent.click(screen.getByTestId('export-data'));
		expect((await screen.findByTestId('export-error')).textContent).toContain(
			'one export every 10 seconds per workspace'
		);
		expect(made).not.toHaveBeenCalled();
	});
});

describe('Account', () => {
	const account = { id: 'a1', email: 'ada@example.com' };
	const shell = (who: typeof account | null) => data({ account: who, invited: 0, scope: 'ws-1', stalePick: false });

	it('gives an account its password form and the way to delete itself', () => {
		render(AccountPage, { props: shell(account) });
		expect(screen.getByTestId('change-password')).toBeTruthy();
		expect(screen.getByTestId('delete-account')).toBeTruthy();
		expect(screen.queryByTestId('account-operator')).toBeNull();
	});

	it('gives the operator an explanation and no form: there is no account to change', () => {
		signInAs('operator');
		render(AccountPage, { props: shell(null) });
		expect(screen.getByTestId('account-operator').textContent).toContain('signed in as the operator');
		expect(screen.queryByTestId('change-password')).toBeNull();
		expect(screen.queryByTestId('delete-account')).toBeNull();
	});

	it('does not call a session the operator just because the account did not load', () => {
		render(AccountPage, { props: shell(null) });
		expect(screen.getByTestId('account-unknown')).toBeTruthy();
		expect(screen.queryByTestId('account-operator')).toBeNull();
		expect(screen.queryByTestId('change-password')).toBeNull();
	});

	it('sends the password change without a workspace pick and says other devices were signed out', async () => {
		pick('ws-1');
		const sent = vi.fn(async (_path: string, _init?: RequestInit) => new Response(null, { status: 204 }));
		vi.stubGlobal('fetch', sent);
		render(AccountPage, { props: shell(account) });
		await fireEvent.input(screen.getByTestId('current-password'), { target: { value: 'the-old-passphrase' } });
		await fireEvent.input(screen.getByTestId('new-password'), { target: { value: 'the-new-passphrase' } });
		await fireEvent.click(screen.getByTestId('change-password'));
		expect((await screen.findByTestId('password-changed')).textContent).toContain(
			'Your other devices were signed out'
		);
		const [path, init] = sent.mock.calls[0] ?? [];
		expect(path).toBe('/api/account/password');
		expect(init?.method).toBe('POST');
		expect(JSON.parse(String(init?.body))).toEqual({
			current_password: 'the-old-passphrase',
			new_password: 'the-new-passphrase'
		});
		expect(new Headers(init?.headers).has('x-smon-workspace')).toBe(false);
	});

	it('says a wrong password plainly and keeps what was typed', async () => {
		vi.stubGlobal(
			'fetch',
			vi.fn(async () =>
				Response.json({ error: { code: 'unauthenticated', message: 'invalid credentials' } }, { status: 401 })
			)
		);
		render(AccountPage, { props: shell(account) });
		await fireEvent.input(screen.getByTestId('current-password'), { target: { value: 'not-the-password' } });
		await fireEvent.input(screen.getByTestId('new-password'), { target: { value: 'the-new-passphrase' } });
		await fireEvent.click(screen.getByTestId('change-password'));
		expect((await screen.findByTestId('password-error')).textContent).toContain("That password isn't right.");
		expect(screen.queryByTestId('password-changed')).toBeNull();
		expect((screen.getByTestId('new-password') as HTMLInputElement).value).toBe('the-new-passphrase');
	});
});

describe('Public status page', () => {
	const service = (name: string, status: 'up' | 'down' | 'unknown') => ({
		name,
		status,
		incident_open_since: status === 'down' ? '2026-11-27T10:00:00Z' : null,
		coverage_bps: 10_000,
		success_bps: status === 'up' ? 10_000 : 0
	});
	const snap = (services: ReturnType<typeof service>[]) => ({
		title: 'Acme status',
		generated_at: new Date().toISOString(),
		expires_at: new Date(Date.now() + 60_000).toISOString(),
		monitoring: 'available' as const,
		services
	});

	it('shows two services that share a name as two rows', () => {
		render(PublicPage, { props: data({ state: 'ok', snap: snap([service('api', 'up'), service('api', 'down')]) }) });
		expect(screen.getAllByTestId('public-service')).toHaveLength(2);
		expect(screen.getByTestId('public-verdict').getAttribute('data-tone')).toBe('attention');
	});

	it('is green only when every service is up', () => {
		render(PublicPage, { props: data({ state: 'ok', snap: snap([service('api', 'up'), service('web', 'up')]) }) });
		expect(screen.getByTestId('public-verdict').getAttribute('data-tone')).toBe('live');
	});

	it('labels every row "last seen" on a stale snapshot and never says up', () => {
		render(PublicPage, { props: data({ state: 'stale', snap: snap([service('api', 'up')]) }) });
		expect(screen.getByTestId('public-verdict').textContent).toContain('Monitoring unavailable');
		const row = screen.getByTestId('public-service');
		expect(row.textContent).toContain('last seen up');
	});

	it('says there is no page here for an unknown address', () => {
		render(PublicPage, { props: data({ state: 'absent', snap: null }) });
		expect(screen.getByTestId('public-absent').textContent).toContain('No status page here');
		expect(screen.queryByTestId('public-verdict')).toBeNull();
	});

	it('shows names only — nothing in the snapshot is rendered as markup', () => {
		const hostile = service('<img src=x onerror=alert(1)>', 'up');
		const { container } = render(PublicPage, { props: data({ state: 'ok', snap: snap([hostile]) }) });
		expect(container.querySelector('img')).toBeNull();
		expect(screen.getByTestId('public-service').textContent).toContain('<img src=x onerror=alert(1)>');
	});
});
