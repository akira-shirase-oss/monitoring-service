import { fireEvent, render, screen } from '@testing-library/svelte';
import { describe, expect, it, vi } from 'vitest';
import Problem from './Problem.svelte';

describe('Problem', () => {
	it('says the monitor cannot see when there was no answer at all', () => {
		render(Problem, { props: { error: null, what: 'monitors' } });
		const panel = screen.getByTestId('problem');
		expect(panel.textContent).toContain("Couldn't load monitors");
		expect(panel.textContent).toContain('this says nothing about your targets');
		// No HTTP status to quote, so no technical details.
		expect(screen.queryByTestId('technical-details')).toBeNull();
	});

	it("shows the server's own words, and the status a report would quote", () => {
		render(Problem, { props: { error: { status: 409, message: 'the workspace needs an owner' } } });
		const panel = screen.getByTestId('problem');
		expect(panel.textContent).toContain('the workspace needs an owner');
		expect(screen.getByTestId('technical-details').textContent).toContain('HTTP 409');
	});

	it('offers sign-in when the session is gone', () => {
		render(Problem, { props: { error: { status: 401, message: 'sign in again' } } });
		expect(screen.getByTestId('problem').textContent).toContain('Sign in to continue');
		expect(screen.getByRole('link', { name: 'Sign in' }).getAttribute('href')).toBe('/login');
	});

	it('says the role cannot do it on a 403, without a sign-in link', () => {
		render(Problem, { props: { error: { status: 403, message: 'owner work' } } });
		expect(screen.getByTestId('problem').textContent).toContain("Your role can't do this");
		expect(screen.queryByRole('link', { name: 'Sign in' })).toBeNull();
	});

	it('offers another try only when another try can help', async () => {
		const retry = vi.fn();
		const { unmount } = render(Problem, { props: { error: { status: 503, message: 'busy' }, retry } });
		await fireEvent.click(screen.getByRole('button', { name: 'Try again' }));
		expect(retry).toHaveBeenCalledTimes(1);
		unmount();

		render(Problem, { props: { error: { status: 429, message: 'slow down' }, retry } });
		expect(screen.getByRole('button', { name: 'Try again' })).toBeTruthy();
	});

	it('does not offer a retry for a refusal that would only repeat', () => {
		render(Problem, { props: { error: { status: 422, message: 'invalid' }, retry: vi.fn() } });
		expect(screen.queryByRole('button', { name: 'Try again' })).toBeNull();
	});

	it('offers a retry when nothing answered', () => {
		render(Problem, { props: { error: null, retry: vi.fn() } });
		expect(screen.getByRole('button', { name: 'Try again' })).toBeTruthy();
	});
});
