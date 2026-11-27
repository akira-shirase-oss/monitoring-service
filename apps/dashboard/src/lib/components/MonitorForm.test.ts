import { fireEvent, render, screen } from '@testing-library/svelte';
import { describe, expect, it, vi } from 'vitest';
import type { MonitorPayload } from '#lib/monitor-form.js';
import MonitorForm from './MonitorForm.svelte';

type Initial = Partial<{
	name: string;
	url: string;
	interval_s: number;
	timeout_s: number;
	failures_to_open: number;
	passes_to_recover: number;
	method: string;
	expect_statuses: string;
	expect_body: string | null;
}>;

/** Renders the form and returns a function that submits it and yields what it sent. */
function setup(props: { initial?: Initial; showIdentity?: boolean } = {}) {
	const onSubmit = vi.fn<(payload: MonitorPayload) => Promise<void>>(async () => {});
	const view = render(MonitorForm, { props: { submitLabel: 'Save', onSubmit, ...props } });
	const submit = async (): Promise<MonitorPayload> => {
		const form = view.container.querySelector('form');
		if (!form) throw new Error('the form did not render');
		await fireEvent.submit(form);
		const payload = onSubmit.mock.calls.at(-1)?.[0];
		if (!payload) throw new Error('the form did not submit');
		return payload;
	};
	return { submit, onSubmit };
}

const type = (testId: string, value: string) =>
	fireEvent.input(screen.getByTestId(testId), { target: { value } });

describe('MonitorForm', () => {
	it('sends only what was filled in, so the server applies its own defaults', async () => {
		const { submit } = setup({ showIdentity: true });
		await fireEvent.input(screen.getByPlaceholderText('billing-api'), { target: { value: '  api  ' } });
		await fireEvent.input(screen.getByPlaceholderText('https://example.com/healthz'), {
			target: { value: 'https://example.com/health' }
		});
		const payload = await submit();
		expect(payload.name).toBe('api');
		expect(payload.url).toBe('https://example.com/health');
		expect(payload.method).toBe('GET');
		expect(payload.interval_s).toBeUndefined();
		expect(payload.timeout_s).toBeUndefined();
		expect(payload.expect_statuses).toBeUndefined();
		expect('expect_body' in payload).toBe(false);
	});

	it('leaves the name and URL out when it is only editing the schedule', async () => {
		const { submit } = setup({ initial: { interval_s: 60, timeout_s: 5 } });
		const payload = await submit();
		expect('name' in payload).toBe(false);
		expect('url' in payload).toBe(false);
		expect(payload.interval_s).toBe(60);
		expect(payload.timeout_s).toBe(5);
	});

	it('parses numbers and drops anything that is not one', async () => {
		const { submit } = setup();
		await fireEvent.input(screen.getByPlaceholderText('60'), { target: { value: ' 30 ' } });
		await fireEvent.input(screen.getByPlaceholderText('5'), { target: { value: 'soon' } });
		const payload = await submit();
		expect(payload.interval_s).toBe(30);
		expect(payload.timeout_s).toBeUndefined();
	});

	it('sends the passing statuses as a list and the body text as written', async () => {
		const { submit } = setup();
		await type('monitor-statuses', '200-204, 418');
		await type('monitor-body', 'ready');
		const payload = await submit();
		expect(payload.expect_statuses).toEqual(['200-204', '418']);
		expect(payload.expect_body).toBe('ready');
	});

	it('locks the body field under HEAD and sends no body text', async () => {
		const { submit } = setup();
		await type('monitor-body', 'ready');
		await fireEvent.change(screen.getByTestId('monitor-method'), { target: { value: 'HEAD' } });
		expect((screen.getByTestId('monitor-body') as HTMLInputElement).disabled).toBe(true);
		const payload = await submit();
		expect(payload.method).toBe('HEAD');
		expect('expect_body' in payload).toBe(false);
	});

	it('clears a stored body expectation with null when switching to HEAD', async () => {
		const { submit } = setup({ initial: { method: 'GET', expect_body: 'ready', expect_statuses: '200-299' } });
		await fireEvent.change(screen.getByTestId('monitor-method'), { target: { value: 'HEAD' } });
		const payload = await submit();
		expect(payload.expect_body).toBeNull();
	});

	it('clears a stored body expectation with null when the field is emptied', async () => {
		const { submit } = setup({ initial: { method: 'GET', expect_body: 'ready' } });
		await type('monitor-body', '');
		const payload = await submit();
		expect(payload.method).toBe('GET');
		expect(payload.expect_body).toBeNull();
	});

	it('starts on what is stored', () => {
		setup({ initial: { method: 'HEAD', expect_statuses: '200-204,418', expect_body: null } });
		expect((screen.getByTestId('monitor-method') as HTMLSelectElement).value).toBe('HEAD');
		expect((screen.getByTestId('monitor-statuses') as HTMLInputElement).value).toBe('200-204,418');
	});

	it('disables the submit button while the save is in flight', async () => {
		let finish: () => void = () => {};
		const onSubmit = vi.fn(() => new Promise<void>((resolve) => (finish = resolve)));
		const view = render(MonitorForm, { props: { submitLabel: 'Save', onSubmit } });
		const form = view.container.querySelector('form');
		if (!form) throw new Error('the form did not render');
		await fireEvent.submit(form);
		const button = screen.getByRole('button', { name: 'Saving…' }) as HTMLButtonElement;
		expect(button.disabled).toBe(true);
		finish();
		await vi.waitFor(() => expect(screen.getByRole('button', { name: 'Save' })).toBeTruthy());
	});
});
