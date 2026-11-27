<script lang="ts">
	import { untrack } from 'svelte';
	import { invalidateAll } from '$app/navigation';
	import { Badge } from '#lib/components/ui/badge/index.js';
	import { Button } from '#lib/components/ui/button/index.js';
	import * as Card from '#lib/components/ui/card/index.js';
	import { Input } from '#lib/components/ui/input/index.js';
	import FormRow from '#lib/components/shell/FormRow.svelte';
	import PageHeader from '#lib/components/shell/PageHeader.svelte';
	import { call } from '#lib/call.js';
	import { span } from '#lib/time.js';
	import { current } from '#lib/workspace.svelte.js';
	import type { PageData } from './$types';

	let { data }: { data: PageData } = $props();

	const who = $derived(current());
	const canWrite = $derived(who?.role === 'owner' || who?.role === 'member');
	const limits = $derived(who?.limits ?? null);

	let name = $state(untrack(() => who?.name ?? ''));
	let busy = $state(false);
	let error = $state<string | null>(null);
	let saved = $state(false);

	const ROLE_WORDS: Record<string, string> = {
		owner: 'Owner: manages people, tokens and everything below.',
		member: 'Member: manages monitors, alerts and the status page.',
		viewer: 'Viewer: reads everything, changes nothing.',
		operator: 'Operator: the server administrator, across every workspace.'
	};

	async function rename(event: SubmitEvent) {
		event.preventDefault();
		busy = true;
		error = null;
		saved = false;
		const answer = await call('/api/workspace', { method: 'PATCH', body: { name: name.trim() } });
		busy = false;
		if (!answer.ok) {
			error = answer.failure.message;
			return;
		}
		saved = true;
		await invalidateAll();
	}
</script>

<svelte:head><title>General · Service Monitor</title></svelte:head>

<PageHeader title="General">
	{#snippet description()}This workspace's name, your role in it, and what its plan allows.{/snippet}
</PageHeader>

<div class="flex flex-col gap-6">
	<Card.Root>
		<Card.Header><Card.Title>Workspace</Card.Title></Card.Header>
		<Card.Content>
			<form onsubmit={rename}>
				<FormRow label="Name" hint="Shown in the workspace menu and in invitations." for="workspace-name">
					<div class="flex flex-col gap-2 sm:flex-row">
						<Input
							id="workspace-name"
							bind:value={name}
							required
							maxlength={80}
							readonly={!canWrite}
							data-testid="workspace-rename"
							oninput={() => (saved = false)}
						/>
						{#if canWrite}
							<Button type="submit" disabled={busy} data-testid="workspace-rename-save">
								{busy ? 'Saving…' : 'Save'}
							</Button>
						{/if}
					</div>
					{#if error}
						<p class="mt-2 text-sm text-attention-ink" role="alert">{error}</p>
					{:else if saved}
						<p class="mt-2 text-sm text-muted-foreground" role="status">Saved.</p>
					{/if}
				</FormRow>
			</form>
			<FormRow label="Your role">
				<p class="text-sm" data-testid="workspace-role">{ROLE_WORDS[who?.role ?? ''] ?? who?.role ?? '—'}</p>
			</FormRow>
		</Card.Content>
	</Card.Root>

	<Card.Root data-testid="plan-card">
		<Card.Header>
			<Card.Title class="flex items-center gap-2">
				Plan <Badge variant="brand" data-testid="plan-name">{who?.plan ?? 'unknown'}</Badge>
			</Card.Title>
			<Card.Description>
				Limits are checked when you save a monitor, and a refusal names the limit it hit.
			</Card.Description>
		</Card.Header>
		<Card.Content>
			{#if limits}
				<dl class="grid gap-x-10 gap-y-4 sm:grid-cols-2" data-testid="plan-limits">
					<div>
						<dt class="text-sm text-muted-foreground">Monitors</dt>
						<dd class="tabular text-sm font-medium">
							{#if limits.max_monitors === null}
								{data.monitorCount ?? '—'} in use · no plan limit
							{:else}
								{data.monitorCount ?? '—'} of {limits.max_monitors} in use
							{/if}
						</dd>
					</div>
					<div>
						<dt class="text-sm text-muted-foreground">Shortest check interval</dt>
						<dd class="tabular text-sm font-medium">{span(limits.min_interval_s * 1000)}</dd>
					</div>
					<div>
						<dt class="text-sm text-muted-foreground">Individual checks kept for</dt>
						<dd class="tabular text-sm font-medium">
							{limits.check_retention_days === null ? "the server's setting" : `${limits.check_retention_days} days`}
						</dd>
					</div>
					<div>
						<dt class="text-sm text-muted-foreground">Recovered incidents kept for</dt>
						<dd class="tabular text-sm font-medium">
							{limits.incident_retention_days === null ? "the server's setting" : `${limits.incident_retention_days} days`}
						</dd>
					</div>
				</dl>
			{:else}
				<p class="text-sm text-muted-foreground">This server didn't report the plan's limits.</p>
			{/if}
		</Card.Content>
	</Card.Root>
</div>
