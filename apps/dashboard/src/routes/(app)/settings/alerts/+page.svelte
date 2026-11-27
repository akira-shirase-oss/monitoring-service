<script lang="ts">
	import CheckIcon from '@lucide/svelte/icons/check';
	import CopyIcon from '@lucide/svelte/icons/copy';
	import { untrack } from 'svelte';
	import { invalidateAll } from '$app/navigation';
	import { Badge } from '#lib/components/ui/badge/index.js';
	import { Button } from '#lib/components/ui/button/index.js';
	import * as Card from '#lib/components/ui/card/index.js';
	import { Input } from '#lib/components/ui/input/index.js';
	import Confirm from '#lib/components/shell/Confirm.svelte';
	import FormRow from '#lib/components/shell/FormRow.svelte';
	import PageHeader from '#lib/components/shell/PageHeader.svelte';
	import Problem from '#lib/components/shell/Problem.svelte';
	import { call, shown } from '#lib/call.js';
	import { current } from '#lib/workspace.svelte.js';
	import type { PageData } from './$types';

	let { data }: { data: PageData } = $props();

	// Saving, testing and removing are member work; a viewer reads what is connected.
	const canWrite = $derived(current()?.role !== 'viewer');

	// The field starts on the stored URL and then belongs to the person typing.
	let url = $state(untrack(() => data.webhook?.url ?? ''));
	let saving = $state(false);
	let saveError = $state<string | null>(null);
	let saved = $state(false);

	let revealed = $state(false);
	let copied = $state(false);

	let testing = $state(false);
	let test = $state<{ ok: boolean; detail: string } | null>(null);
	let testError = $state<string | null>(null);

	let removeOpen = $state(false);
	let removeBusy = $state(false);
	let removeError = $state<string | null>(null);

	// What the receiver said, in plain words. The wire word stays visible beside it —
	// it is what the logs and the docs use.
	const TEST_WORDS: Record<string, string> = {
		delivered: 'Your endpoint answered with a success status.',
		rejected_local: 'The address points at a private or local network, which alerts are never sent to.',
		deadline: 'Your endpoint did not answer in time.',
		dns: "The hostname didn't resolve.",
		connect: 'The connection was refused or could not be opened.',
		tls: 'The TLS handshake failed.',
		redirect: 'Your endpoint answered with a redirect. Alerts are sent to the exact address only.'
	};

	function explain(detail: string): string {
		if (TEST_WORDS[detail]) return TEST_WORDS[detail];
		const status = /^http_(\d{3})$/.exec(detail);
		if (status) return `Your endpoint answered HTTP ${status[1]}, which counts as a failed delivery.`;
		return 'The delivery failed.';
	}

	async function save(event: SubmitEvent) {
		event.preventDefault();
		saving = true;
		saveError = null;
		saved = false;
		test = null;
		const answer = await call('/api/workspace/webhook', { method: 'PUT', body: { url: url.trim() } });
		saving = false;
		if (!answer.ok) {
			saveError = answer.failure.message;
			return;
		}
		saved = true;
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

	async function copySecret() {
		if (!data.webhook) return;
		try {
			await navigator.clipboard.writeText(data.webhook.secret);
			copied = true;
		} catch {
			revealed = true;
		}
	}

	async function remove() {
		removeBusy = true;
		removeError = null;
		const answer = await call('/api/workspace/webhook', { method: 'DELETE' });
		removeBusy = false;
		if (!answer.ok) {
			removeError = answer.failure.message;
			return;
		}
		removeOpen = false;
		url = '';
		test = null;
		saved = false;
		await invalidateAll();
	}
</script>

<svelte:head><title>Alerts · Service Monitor</title></svelte:head>

<PageHeader title="Alerts">
	{#snippet description()}
		When an incident opens or recovers, Service Monitor sends a signed POST to your webhook.
	{/snippet}
	{#snippet meta()}
		{#if !data.failure}
			{#if data.webhook}
				<Badge variant="live" data-testid="webhook-state">Connected</Badge>
			{:else}
				<Badge variant="draft" data-testid="webhook-state">No alert channel connected</Badge>
			{/if}
		{/if}
	{/snippet}
</PageHeader>

<div class="flex flex-col gap-6">
	{#if data.failure}
		<Problem error={shown(data.failure)} what="the alert settings" retry={invalidateAll} />
	{:else}
		<Card.Root>
			<Card.Header>
				<Card.Title>Webhook</Card.Title>
				<Card.Description>
					A public HTTPS address you control. It is checked when you save it and again before
					every send; private and local addresses are refused.
				</Card.Description>
			</Card.Header>
			<Card.Content>
				<form onsubmit={save}>
					<FormRow label="Address" hint="Each alert is one JSON POST to this URL." for="webhook-url">
						<div class="flex flex-col gap-2 sm:flex-row">
							<Input
								id="webhook-url"
								type="url"
								bind:value={url}
								required
								readonly={!canWrite}
								class="font-mono text-sm"
								placeholder="https://hooks.example.com/service-monitor"
								data-testid="webhook-url"
							/>
							{#if canWrite}
								<Button type="submit" disabled={saving} data-testid="webhook-save">
									{saving ? 'Saving…' : data.webhook ? 'Save' : 'Connect'}
								</Button>
							{/if}
						</div>
						{#if saveError}
							<p class="mt-2 text-sm text-attention-ink" role="alert" data-testid="webhook-error">
								{saveError}
							</p>
						{:else if saved}
							<p class="mt-2 text-sm text-muted-foreground" role="status">Saved.</p>
						{/if}
					</FormRow>
				</form>

				{#if data.webhook}
					<FormRow
						label="Signing secret"
						hint="Each request carries x-smon-signature: sha256=<HMAC of the body with this secret>. Verify it before you trust an alert."
					>
						<div class="flex flex-col gap-2 sm:flex-row">
							<Input
								readonly
								type={revealed ? 'text' : 'password'}
								value={data.webhook.secret}
								class="font-mono text-sm"
								aria-label="Signing secret"
								data-testid="webhook-secret"
							/>
							<Button variant="outline" onclick={() => (revealed = !revealed)}>
								{revealed ? 'Hide' : 'Show'}
							</Button>
							<Button variant="outline" onclick={copySecret}>
								{#if copied}<CheckIcon /> Copied{:else}<CopyIcon /> Copy{/if}
							</Button>
						</div>
					</FormRow>

					{#if canWrite}
						<FormRow
							label="Test alert"
							hint="Sends one signed test event now, through the same checks as a real alert."
						>
							<div class="flex flex-col items-start gap-2">
								<Button variant="outline" onclick={sendTest} disabled={testing} data-testid="webhook-test">
									{testing ? 'Sending…' : 'Send test alert'}
								</Button>
								{#if testError}
									<p class="text-sm text-attention-ink" role="alert">{testError}</p>
								{:else if test}
									<p class="flex flex-wrap items-center gap-2 text-sm" role="status" data-testid="webhook-test-result">
										<Badge variant={test.ok ? 'live' : 'attention'}>{test.ok ? 'Delivered' : 'Not delivered'}</Badge>
										<span>{explain(test.detail)}</span>
										<code class="font-mono text-xs text-muted-foreground">{test.detail}</code>
									</p>
								{/if}
							</div>
						</FormRow>

						<FormRow label="Disconnect" hint="Stops alerts for this workspace until a webhook is connected again.">
							<Button
								variant="outline"
								data-testid="webhook-remove"
								onclick={() => {
									removeError = null;
									removeOpen = true;
								}}>Remove webhook</Button
							>
						</FormRow>
					{/if}
				{/if}
			</Card.Content>
		</Card.Root>
	{/if}
</div>

<Confirm
	bind:open={removeOpen}
	title="Remove the webhook?"
	confirm="Remove webhook"
	busy={removeBusy}
	onConfirm={remove}
>
	Incidents will still be recorded, but nobody will be alerted. A webhook connected later gets a
	new signing secret.
	{#if removeError}<span class="mt-2 block text-attention-ink" role="alert">{removeError}</span>{/if}
</Confirm>
