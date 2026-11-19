<script lang="ts">
	import * as Card from '#lib/components/ui/card/index.js';
	import { Button } from '#lib/components/ui/button/index.js';
	import { TriangleAlert, Radar } from '@lucide/svelte';
	import { onMount } from 'svelte';
	import { invalidateAll } from '$app/navigation';
	import MonitorForm from '#lib/components/MonitorForm.svelte';
	import EmptyState from '#lib/components/shell/EmptyState.svelte';
	import PageHeader from '#lib/components/shell/PageHeader.svelte';
	import MonitorBadge from '#lib/components/shell/MonitorBadge.svelte';
	import Problem from '#lib/components/shell/Problem.svelte';
	import { apiFetch } from '#lib/auth.js';
	import { readEnvelope } from '#lib/api.js';
	import type { MonitorPayload } from '#lib/monitor-form.js';
	import type { PageData } from './$types';
	import type { MonitorView } from './+page';

	let { data }: { data: PageData } = $props();

	// Poll on the minimum check interval — SvelteKit holds the previous data until the
	// reload resolves, so the swap is atomic: a commit mid-poll flips the badge once,
	// an API outage mid-poll resolves to "cannot see", and nothing ever flashes stale.
	onMount(() => {
		const timer = setInterval(() => {
			if (document.visibilityState === 'visible') invalidateAll();
		}, 15_000);
		return () => clearInterval(timer);
	});

	let adding = $state(false);
	let addError = $state<string | null>(null);
	let backupConfirm = $state(false);
	let backupBusy = $state(false);
	let backupNote = $state<string | null>(null);

	// Operator snapshot: two clicks like delete — a backup takes real disk work, and the
	// confirmation names what the second click does. The server names the file
	// (SMON_BACKUP_DIR); this only echoes what it reports back.
	async function backUp() {
		if (!backupConfirm) {
			backupConfirm = true;
			return;
		}
		backupConfirm = false;
		backupBusy = true;
		try {
			const response = await apiFetch(
				'/api/backup',
				{
					method: 'POST',
					headers: { 'content-type': 'application/json' },
					body: '{}'
				},
				fetch
			);
			const failure = await readEnvelope(response);
			if (failure) {
				backupNote =
					failure.status === 404
						? 'No backup directory is configured on this server (SMON_BACKUP_DIR unset).'
						: failure.message;
				return;
			}
			const created = (await response.json()) as { name?: string; bytes?: number };
			backupNote = `saved ${created.name ?? '(unnamed)'} (${Number(created.bytes ?? 0).toLocaleString()} bytes)`;
		} catch {
			backupNote = 'The monitor server is unreachable.';
		} finally {
			backupBusy = false;
		}
	}

	async function create(payload: MonitorPayload) {
		const response = await apiFetch(
			'/api/monitors',
			{ method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(payload) },
			fetch
		);
		const failure = await readEnvelope(response);
		if (failure) {
			addError =
				failure.status === 401
					? 'Your session ended — sign in again.'
					: failure.message;
			return;
		}
		addError = null;
		adding = false;
		await invalidateAll();
	}
</script>

<svelte:head><title>Monitors · Service Monitor</title></svelte:head>

<div class="flex flex-col gap-6">
	<PageHeader title="Monitors">
		{#snippet description()}
			Authorized HTTP targets, checked on a schedule. Status here is evidence, not hope.
		{/snippet}
		{#snippet actions()}
			{#if data.reachable && !data.needsAuth}
				<Button variant="outline" size="sm" onclick={backUp} disabled={backupBusy}>
					{backupBusy ? 'Backing up…' : backupConfirm ? 'Confirm backup' : 'Back up now'}
				</Button>
				{#if !adding}
					<Button size="sm" onclick={() => (adding = true)} data-testid="new-monitor">Add monitor</Button>
				{/if}
			{/if}
		{/snippet}
	</PageHeader>
	{#if backupNote}
		<p class="-mt-4 text-right text-xs text-muted-foreground">{backupNote}</p>
	{/if}

	{#if data.needsAuth}
		<Problem error={{ status: 401, message: 'Your session ended — sign in again.' }} what="monitors" />
	{:else if !data.reachable}
		<Card.Root class="border-attention/40 bg-attention-soft">
			<Card.Content class="flex items-start gap-3 p-4">
				<TriangleAlert class="mt-0.5 size-4 shrink-0 text-attention-ink" />
				<div>
					<p class="text-sm font-medium text-attention-ink">The monitor server is unreachable.</p>
					<p class="mt-0.5 text-sm text-attention-ink/80">
						This says nothing about your targets — it means the monitor cannot see right now.
					</p>
				</div>
			</Card.Content>
		</Card.Root>
	{:else}
		{#if data.monitoring && data.monitoring.status !== 'ok'}
			<Card.Root class="border-attention/40 bg-attention-soft">
				<Card.Content class="flex items-start gap-3 p-4">
					<TriangleAlert class="mt-0.5 size-4 shrink-0 text-attention-ink" />
					<div>
						<p class="text-sm font-medium text-attention-ink">
							{data.monitoring.status === 'unavailable'
								? 'Monitoring is unavailable.'
								: 'Monitoring is degraded.'}
						</p>
						<p class="mt-0.5 text-sm text-attention-ink/80">
							{data.monitoring.status === 'unavailable'
								? 'The monitor is not observing right now, so every status below reads unknown. This says nothing about your targets.'
								: 'Checks are still running, but part of the monitor has stopped.'}
							{#if data.monitoring.faults.length > 0}
								Cause: {data.monitoring.faults.join('; ')}.
							{/if}
						</p>
					</div>
				</Card.Content>
			</Card.Root>
		{/if}
		{#if adding}
			<Card.Root>
				<Card.Content class="p-4">
					<MonitorForm
						showIdentity
						submitLabel="Create monitor"
						onSubmit={create}
						onCancel={() => {
							adding = false;
							addError = null;
						}}
					/>
					{#if addError}
						<p class="mt-3 text-sm text-attention-ink" role="alert">{addError}</p>
					{/if}
				</Card.Content>
			</Card.Root>
		{/if}
		{#if data.monitors.length === 0 && !adding}
			<EmptyState icon={Radar} title="Nothing watched yet" testid="empty-monitors">
				Register the first target — a health URL you own, checked on the interval you set.
				{#snippet action()}
					<Button size="sm" onclick={() => (adding = true)}>Watch your first target</Button>
				{/snippet}
			</EmptyState>
		{:else}
			<div class="stagger flex flex-col gap-3">
				{#each data.monitors as monitor, i (monitor.id)}
					<Card.Root style="--i:{i}">
						<Card.Content class="flex items-center justify-between gap-4 p-4">
							<div class="min-w-0">
								<a
									href="/monitors/{monitor.id}"
									class="block truncate font-medium hover:underline"
								>
									{monitor.name}
								</a>
								<p class="truncate font-mono text-xs text-muted-foreground">{monitor.url}</p>
							</div>
							<div class="flex shrink-0 items-center gap-3">
								<span class="tabular text-sm text-muted-foreground"
									>every {monitor.interval_s}s</span
								>
								<MonitorBadge status={monitor.status} paused={monitor.paused} />
							</div>
						</Card.Content>
					</Card.Root>
				{/each}
			</div>
		{/if}
	{/if}
</div>
