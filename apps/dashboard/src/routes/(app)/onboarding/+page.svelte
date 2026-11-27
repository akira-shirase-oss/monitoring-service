<script lang="ts">
	import CheckIcon from '@lucide/svelte/icons/check';
	import { onMount, untrack } from 'svelte';
	import { goto, invalidateAll } from '$app/navigation';
	import { Badge } from '#lib/components/ui/badge/index.js';
	import { Button } from '#lib/components/ui/button/index.js';
	import * as Card from '#lib/components/ui/card/index.js';
	import { Input } from '#lib/components/ui/input/index.js';
	import { Spinner } from '#lib/components/ui/spinner/index.js';
	import MonitorForm from '#lib/components/MonitorForm.svelte';
	import PageHeader from '#lib/components/shell/PageHeader.svelte';
	import Problem from '#lib/components/shell/Problem.svelte';
	import { call, shown } from '#lib/call.js';
	import { dismissGuide } from '#lib/guide.svelte.js';
	import type { MonitorPayload } from '#lib/monitor-form.js';
	import { current } from '#lib/workspace.svelte.js';
	import type { PageData } from './$types';

	let { data }: { data: PageData } = $props();

	const DEFAULT_NAME = 'Personal workspace';
	const who = $derived(current());
	const canWrite = $derived(who?.role !== 'viewer');

	const named = $derived((who?.name ?? DEFAULT_NAME) !== DEFAULT_NAME);
	const watching = $derived(data.monitors.length > 0);
	const alerting = $derived(data.webhookUrl !== null);
	const observed = $derived(data.observed > 0);
	// Naming the workspace is a courtesy; the three that matter are a target, somewhere
	// to send the alert, and the first evidence.
	const complete = $derived(watching && alerting && observed);

	// While the first check is still on its way, look again every few seconds.
	onMount(() => {
		const timer = setInterval(() => {
			if (document.visibilityState === 'visible' && watching && !observed) invalidateAll();
		}, 5_000);
		return () => clearInterval(timer);
	});

	let name = $state(untrack(() => (named ? (who?.name ?? '') : '')));
	let nameBusy = $state(false);
	let nameError = $state<string | null>(null);

	let monitorError = $state<string | null>(null);

	let url = $state('');
	let hookBusy = $state(false);
	let hookError = $state<string | null>(null);
	let testing = $state(false);
	let test = $state<{ ok: boolean; detail: string } | null>(null);
	let testError = $state<string | null>(null);

	async function rename(event: SubmitEvent) {
		event.preventDefault();
		nameBusy = true;
		nameError = null;
		const answer = await call('/api/workspace', { method: 'PATCH', body: { name: name.trim() } });
		nameBusy = false;
		if (!answer.ok) {
			nameError = answer.failure.message;
			return;
		}
		await invalidateAll();
	}

	async function watch(payload: MonitorPayload) {
		monitorError = null;
		const answer = await call('/api/monitors', { method: 'POST', body: payload });
		if (!answer.ok) {
			monitorError = answer.failure.message;
			return;
		}
		await invalidateAll();
	}

	async function connect(event: SubmitEvent) {
		event.preventDefault();
		hookBusy = true;
		hookError = null;
		const answer = await call('/api/workspace/webhook', { method: 'PUT', body: { url: url.trim() } });
		hookBusy = false;
		if (!answer.ok) {
			hookError = answer.failure.message;
			return;
		}
		await invalidateAll();
	}

	async function sendTest() {
		testing = true;
		test = null;
		testError = null;
		const answer = await call<{ test: { ok: boolean; detail: string } }>(
			'/api/workspace/webhook/test',
			{ method: 'POST', body: {} }
		);
		testing = false;
		if (!answer.ok) {
			testError = answer.failure.message;
			return;
		}
		test = answer.body.test;
	}

	async function finish() {
		dismissGuide(who?.id ?? '');
		await goto('/');
	}
</script>

<svelte:head><title>Get started · Service Monitor</title></svelte:head>

<PageHeader title="Get started">
	{#snippet description()}
		Four steps from an empty workspace to a monitor that pages you. Each one ticks itself when
		the workspace really has it.
	{/snippet}
	{#snippet actions()}
		<Button variant={complete ? 'default' : 'outline'} size="sm" onclick={finish} data-testid="guide-finish">
			{complete ? 'Done, go to monitors' : 'Skip for now'}
		</Button>
	{/snippet}
</PageHeader>

{#snippet step(n: number, done: boolean, title: string, optional: boolean)}
	<div class="flex items-center gap-3">
		<span
			class="flex size-7 shrink-0 items-center justify-center rounded-full border text-sm font-medium {done
				? 'border-transparent bg-live-soft text-live-ink'
				: 'border-border bg-popover text-muted-foreground'}"
			aria-hidden="true"
		>
			{#if done}<CheckIcon class="size-4" />{:else}{n}{/if}
		</span>
		<Card.Title class="flex flex-wrap items-center gap-2">
			{title}
			{#if done}
				<Badge variant="live">Done</Badge>
			{:else if optional}
				<Badge variant="draft">Optional</Badge>
			{/if}
		</Card.Title>
	</div>
{/snippet}

<div class="flex flex-col gap-4">
	{#if data.failure}
		<Problem error={shown(data.failure)} what="your workspace" retry={invalidateAll} />
	{:else}
		<Card.Root data-testid="step-name" data-done={named}>
			<Card.Header>
				{@render step(1, named, 'Name the workspace', true)}
				<Card.Description>The name people see in invitations and in the workspace menu.</Card.Description>
			</Card.Header>
			{#if canWrite}
				<Card.Content>
					<form class="flex flex-col gap-2 sm:flex-row" onsubmit={rename}>
						<Input bind:value={name} required maxlength={80} placeholder="Acme production" aria-label="Workspace name" data-testid="guide-name" />
						<Button type="submit" variant="outline" disabled={nameBusy} data-testid="guide-name-save">
							{nameBusy ? 'Saving…' : named ? 'Rename' : 'Save name'}
						</Button>
					</form>
					{#if nameError}<p class="mt-2 text-sm text-attention-ink" role="alert">{nameError}</p>{/if}
				</Card.Content>
			{/if}
		</Card.Root>

		<Card.Root data-testid="step-monitor" data-done={watching}>
			<Card.Header>
				{@render step(2, watching, 'Watch your first URL', false)}
				<Card.Description>
					{#if watching}
						Watching <a class="font-medium underline" href="/monitors/{data.monitors[0]?.id}">{data.monitors[0]?.name}</a>{data.monitors.length > 1 ? ` and ${data.monitors.length - 1} more` : ''}.
					{:else}
						A health URL you own. It is requested on the interval you set; leave the numbers
						empty to use the defaults.
					{/if}
				</Card.Description>
			</Card.Header>
			{#if !watching && canWrite}
				<Card.Content>
					<MonitorForm showIdentity submitLabel="Start watching" onSubmit={watch} />
					{#if monitorError}
						<p class="mt-3 text-sm text-attention-ink" role="alert" data-testid="guide-monitor-error">{monitorError}</p>
					{/if}
				</Card.Content>
			{/if}
		</Card.Root>

		<Card.Root data-testid="step-alerts" data-done={alerting}>
			<Card.Header>
				{@render step(3, alerting, 'Connect an alert channel', false)}
				<Card.Description>
					{#if alerting}
						Alerts go to <code class="font-mono text-xs">{data.webhookUrl}</code>. The signing secret
						is on the <a class="underline" href="/settings/alerts">Alerts</a> page.
					{:else}
						A webhook that receives a signed POST when an incident opens or recovers.
					{/if}
				</Card.Description>
			</Card.Header>
			{#if canWrite}
				<Card.Content>
					{#if alerting}
						<div class="flex flex-wrap items-center gap-3">
							<Button variant="outline" onclick={sendTest} disabled={testing} data-testid="guide-test">
								{testing ? 'Sending…' : 'Send test alert'}
							</Button>
							{#if testError}
								<p class="text-sm text-attention-ink" role="alert">{testError}</p>
							{:else if test}
								<p class="flex items-center gap-2 text-sm" role="status" data-testid="guide-test-result">
									<Badge variant={test.ok ? 'live' : 'attention'}>{test.ok ? 'Delivered' : 'Not delivered'}</Badge>
									<code class="font-mono text-xs text-muted-foreground">{test.detail}</code>
								</p>
							{/if}
						</div>
					{:else}
						<form class="flex flex-col gap-2 sm:flex-row" onsubmit={connect}>
							<Input type="url" bind:value={url} required class="font-mono text-sm" placeholder="https://hooks.example.com/service-monitor" aria-label="Webhook address" data-testid="guide-webhook" />
							<Button type="submit" variant="outline" disabled={hookBusy} data-testid="guide-webhook-save">
								{hookBusy ? 'Connecting…' : 'Connect'}
							</Button>
						</form>
						{#if hookError}<p class="mt-2 text-sm text-attention-ink" role="alert" data-testid="guide-webhook-error">{hookError}</p>{/if}
					{/if}
				</Card.Content>
			{/if}
		</Card.Root>

		<Card.Root data-testid="step-checks" data-done={observed}>
			<Card.Header>
				{@render step(4, observed, 'See the first checks land', false)}
				<Card.Description>
					{#if observed}
						{data.observed} {data.observed === 1 ? 'check has' : 'checks have'} been recorded for
						{data.monitors[0]?.name}. Its status is now evidence, not a guess.
					{:else if watching}
						<span class="inline-flex items-center gap-2"><Spinner class="size-3.5" /> Waiting for the first check of {data.monitors[0]?.name}. It runs within one interval.</span>
					{:else}
						Starts by itself once a monitor exists.
					{/if}
				</Card.Description>
			</Card.Header>
		</Card.Root>
	{/if}
</div>
