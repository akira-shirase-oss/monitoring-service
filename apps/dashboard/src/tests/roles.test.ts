// What each role is offered on the settings pages. The server decides what a role may
// do; these pages must not offer a control the server will refuse, and must not hide
// from an owner what only an owner can do.
import { render, screen } from '@testing-library/svelte';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { pick, workspace } from '#lib/workspace.svelte.js';
import MembersPage from '../routes/(app)/settings/members/+page.svelte';
import StatusPage from '../routes/(app)/settings/status-page/+page.svelte';
import TokensPage from '../routes/(app)/settings/tokens/+page.svelte';

vi.mock('$app/navigation', () => ({ invalidateAll: vi.fn(async () => {}), goto: vi.fn(async () => {}) }));

function signInAs(role: string) {
	workspace.list = [{ id: 'ws-1', slug: 'acme', name: 'Acme', role, plan: 'free', limits: null }];
	workspace.home = 'ws-1';
	pick('');
}

const token = {
	id: 't1',
	name: 'deploy script',
	role: 'viewer' as const,
	created_at: '2026-11-27T10:00:00Z',
	revoked_at: null,
	last_used_at: null
};
const member = { account: 'a1', email: 'owner@example.com', role: 'owner' as const, created_at: '2026-11-27T10:00:00Z' };
const statusPage = { slug: 'acme', title: 'Acme status', monitor_ids: ['m1'] };
const monitors = [{ id: 'm1', name: 'api', url: 'https://example.com/health' }];

// SvelteKit hands a page its load data plus layout data; these pages read only their own.
const data = <T>(own: T) => ({ data: own }) as never;

beforeEach(() => signInAs('owner'));

describe('API tokens', () => {
	it('lets an owner create and revoke', () => {
		render(TokensPage, { props: data({ tokens: [token], failure: null }) });
		expect(screen.getByTestId('new-token')).toBeTruthy();
		expect(screen.getByTestId('token-revoke')).toBeTruthy();
	});

	it('shows a member the list and no controls', () => {
		signInAs('member');
		render(TokensPage, { props: data({ tokens: [token], failure: null }) });
		expect(screen.getByTestId('token-row').textContent).toContain('deploy script');
		expect(screen.queryByTestId('new-token')).toBeNull();
		expect(screen.queryByTestId('token-revoke')).toBeNull();
	});

	it('marks a revoked token as revoked and offers nothing more to do with it', () => {
		render(TokensPage, {
			props: data({ tokens: [{ ...token, revoked_at: '2026-11-27T11:00:00Z' }], failure: null })
		});
		expect(screen.getByTestId('token-row').textContent).toContain('Revoked');
		expect(screen.queryByTestId('token-revoke')).toBeNull();
	});

	it('never shows a secret in the list', () => {
		render(TokensPage, { props: data({ tokens: [token], failure: null }) });
		expect(document.body.textContent).not.toContain('smon_');
		expect(screen.queryByTestId('token-secret')).toBeNull();
	});

	it("shows the server's refusal instead of an empty list", () => {
		render(TokensPage, {
			props: data({ tokens: [], failure: { status: 403, message: 'not for this role' } })
		});
		expect(screen.getByTestId('problem').textContent).toContain('not for this role');
		expect(screen.queryByTestId('empty-tokens')).toBeNull();
	});
});

describe('Members', () => {
	const page = { members: [member], sent: [], received: [], failure: null };

	it('offers an owner the invite form, with viewer and member only', () => {
		render(MembersPage, { props: data(page) });
		const roles = [...(screen.getByTestId('invite-role') as HTMLSelectElement).options].map((o) => o.value);
		// An invitation can't make an owner: the server refuses it.
		expect(roles).toEqual(['viewer', 'member']);
		expect(screen.getByTestId('member-remove')).toBeTruthy();
	});

	it('shows a viewer the roster read-only', () => {
		signInAs('viewer');
		render(MembersPage, { props: data(page) });
		expect(screen.getByTestId('member-row').textContent).toContain('owner@example.com');
		expect(screen.queryByTestId('invite-email')).toBeNull();
		expect(screen.queryByTestId('member-role')).toBeNull();
		expect(screen.queryByTestId('member-remove')).toBeNull();
	});

	it('shows invitations addressed to the account, whatever its role here', () => {
		signInAs('viewer');
		render(MembersPage, {
			props: data({
				...page,
				received: [{ id: 'i1', workspace: 'Globex', role: 'member', created_at: '2026-11-27T10:00:00Z' }]
			})
		});
		expect(screen.getByTestId('received-invitations').textContent).toContain('Globex');
		expect(screen.getByTestId('invitation-accept')).toBeTruthy();
	});
});

describe('Status page editor', () => {
	const page = { page: statusPage, monitors, failure: null };

	it('lets an owner save', () => {
		render(StatusPage, { props: data(page) });
		expect(screen.getByTestId('status-save')).toBeTruthy();
		expect(screen.queryByTestId('status-readonly')).toBeNull();
		expect((screen.getByTestId('status-title') as HTMLInputElement).readOnly).toBe(false);
	});

	it('does not offer a member a save the server would refuse', () => {
		signInAs('member');
		render(StatusPage, { props: data(page) });
		expect(screen.queryByTestId('status-save')).toBeNull();
		expect(screen.getByTestId('status-readonly')).toBeTruthy();
		expect((screen.getByTestId('status-title') as HTMLInputElement).readOnly).toBe(true);
	});

	it('says Public only when the stored page has a title', () => {
		const { unmount } = render(StatusPage, { props: data(page) });
		expect(screen.getByTestId('status-page-state').textContent).toBe('Public');
		unmount();
		render(StatusPage, { props: data({ ...page, page: { ...statusPage, title: null } }) });
		expect(screen.getByTestId('status-page-state').textContent).toBe('Off');
	});

	it('links to the page by its slug and lists monitors by name', () => {
		render(StatusPage, { props: data(page) });
		expect(screen.getByTestId('status-page-link').getAttribute('href')).toBe('/s/acme');
		expect(screen.getByTestId('status-monitors').textContent).toContain('api');
	});
});
