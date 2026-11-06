<script lang="ts">
	import { Badge, type BadgeVariant } from '#lib/components/ui/badge/index.js';
	import * as Card from '#lib/components/ui/card/index.js';
	import { ArrowLeft, TriangleAlert } from '@lucide/svelte';
	import { onMount } from 'svelte';
	import { goto, invalidateAll } from '$app/navigation';
	import LoginGate from '#lib/components/LoginGate.svelte';
	import MonitorForm from '#lib/components/MonitorForm.svelte';
	import CheckStrip from '#lib/components/CheckStrip.svelte';
	import { apiFetch } from '#lib/auth.js';
	import { readEnvelope } from '#lib/api.js';
	import type { MonitorPayload } from '#lib/monitor-form.js';
	import type { PageData } from './$types';
	import type { CheckView } from './+page';

	let { data }: { data: PageData } = $props();

	// Same honest poll as the list: atomic data swap, API outage -> unreachable,
	// hidden tabs don't poll.
	onMount(() => {
		const timer = setInterval(() => {
			if (document.visibilityState === 'visible') invalidateAll();
		}, 15_000);
		return () => clearInterval(timer);
	});

	let editing = $state(false);
	let confirmingDelete = $state(false);
	let actionError = $state<string | null>(null);
	let actionBusy = $state(false);

	// Every management write shows the server's own error — a 409 conflict is real
	// information (stale read, or the monitor has history), never something to swallow.
	async function mutate(id: string, init: RequestInit): Promise<boolean> {
		actionBusy = true;
		try {
			const response = await apiFetch(`/api/monitors/${id}`, init, fetch);
			const failure = await readEnvelope(response);
			if (failure) {
				actionError = failure.message;
				return false;
			}
			actionError = null;
			return true;
		} catch {
			actionError = 'The monitor server is unreachable.';
			return false;
		} finally {
			actionBusy = false;
		}
	}

	async function togglePaused(id: string, paused: boolean) {
		if (await mutate(id, {
			method: 'PATCH',
			headers: { 'content-type': 'application/json' },
			body: JSON.stringify({ paused })
		})) await invalidateAll();
	}

	async function saveEdit(id: string, generation: number, payload: MonitorPayload) {
		if (await mutate(id, {
			method: 'PATCH',
			headers: { 'content-type': 'application/json' },
			body: JSON.stringify({ ...payload, generation })
		})) {
			editing = false;
			await invalidateAll();
		}
	}

	async function remove(id: string) {
		if (!confirmingDelete) {
			confirmingDelete = true;
			return;
		}
		if (await mutate(id, { method: 'DELETE' })) await goto('/');
	}

	function variantFor(outcome: CheckView['outcome']): BadgeVariant {
		switch (outcome) {
			case 'pass':
				return 'up';
			case 'missed':
				return 'unknown';
			default:
				return 'down';
		}
	}

	function labelFor(check: CheckView): string {
		if (check.outcome === 'http_fail' && check.http_status) {
			return `HTTP ${check.http_status}`;
		}
		return check.outcome;
	}

	// Basis points → percent, one decimal. `null` means undefined denominator — display
	// "no data", never a guessed number.
	function pct(bps: number | null): string {
		return bps == null ? 'no data' : `${(bps / 100).toFixed(1)}%`;
	}
</script>

<svelte:head>
	<title>{data.state === 'ok' ? data.monitor.name : 'Monitor'} · Service Monitor</title>
</svelte:head>

<div class="flex flex-col gap-6">
	<a
		href="/"
		class="inline-flex w-fit items-center gap-1.5 text-sm text-muted-foreground hover:text-foreground"
	>
		<ArrowLeft class="size-4" /> Monitors
	</a>

	{#if data.state === 'needsAuth'}
		<LoginGate />
	{:else if data.state === 'missing'}
		<Card.Root class="border-dashed">
			<Card.Content class="p-10 text-center">
				<p class="font-medium">No monitor with that id.</p>
				<p class="mt-1 text-sm text-muted-foreground">It may have been deleted.</p>
			</Card.Content>
		</Card.Root>
	{:else if data.state === 'unreachable'}
		<Card.Root class="border-attention/40 bg-attention-soft">
			<Card.Content class="flex items-start gap-3 p-4">
				<TriangleAlert class="mt-0.5 size-4 shrink-0 text-attention-ink" />
				<div>
					<p class="text-sm font-medium text-attention-ink">The monitor server is unreachable.</p>
					<p class="mt-0.5 text-sm text-attention-ink/80">
						This says nothing about the target — the monitor cannot see right now.
					</p>
				</div>
			</Card.Content>
		</Card.Root>
	{:else}
		<div>
			<div class="flex items-center gap-3">
				<h1 class="text-2xl font-semibold tracking-tight">{data.monitor.name}</h1>
				<Badge variant={data.monitor.paused ? 'secondary' : (data.monitor.status as BadgeVariant)}>
					{data.monitor.paused ? 'paused' : data.monitor.status}
				</Badge>
			</div>
			<p class="mt-1 truncate font-mono text-sm text-muted-foreground">{data.monitor.url}</p>
			<p class="mt-0.5 text-sm text-muted-foreground">
				every {data.monitor.interval_s}s · deadline {data.monitor.timeout_s}s · opens after {data
					.monitor.failures_to_open} fails · recovers after {data.monitor.passes_to_recover} passes
				· generation {data.monitor.generation}
			</p>
			<div class="mt-3 flex flex-wrap items-center gap-2">
				<button
					onclick={() => togglePaused(data.monitor.id, !data.monitor.paused)}
					disabled={actionBusy}
					class="rounded-md border border-input px-3 py-1.5 text-sm disabled:opacity-50"
				>
					{data.monitor.paused ? 'Resume' : 'Pause'}
				</button>
				<button
					onclick={() => {
						editing = !editing;
						actionError = null;
					}}
					disabled={actionBusy}
					class="rounded-md border border-input px-3 py-1.5 text-sm disabled:opacity-50"
				>
					{editing ? 'Close edit' : 'Edit'}
				</button>
				<button
					onclick={() => remove(data.monitor.id)}
					disabled={actionBusy}
					class="rounded-md border border-down/40 px-3 py-1.5 text-sm text-down disabled:opacity-50"
				>
					{confirmingDelete ? 'Confirm delete' : 'Delete'}
				</button>
			</div>
			{#if actionError}
				<p class="mt-2 text-sm text-attention-ink">{actionError}</p>
			{/if}
			{#if editing}
				<Card.Root class="mt-3">
					<Card.Content class="p-4">
						<MonitorForm
							initial={{
								interval_s: data.monitor.interval_s,
								timeout_s: data.monitor.timeout_s,
								failures_to_open: data.monitor.failures_to_open,
								passes_to_recover: data.monitor.passes_to_recover
							}}
							submitLabel="Save changes"
							onSubmit={(payload) => saveEdit(data.monitor.id, data.monitor.generation, payload)}
							onCancel={() => (editing = false)}
						/>
					</Card.Content>
				</Card.Root>
			{/if}
			<p class="mt-0.5 text-sm text-muted-foreground">
				last {Math.round(data.stats.window_s / 3600)}h · coverage {pct(data.stats.coverage_bps)}
				({data.stats.observed}/{data.stats.expected} slots{data.stats.missed > 0
					? `, ${data.stats.missed} missed`
					: ''}) · success {pct(data.stats.success_bps)}
			</p>
		</div>

		{#if data.windows.length > 0}
			<div>
				<h2 class="text-sm font-medium text-muted-foreground">Maintenance windows</h2>
				<div class="mt-2 flex flex-col gap-1.5">
					{#each data.windows as window (window.id)}
						<div
							class="flex items-center justify-between gap-4 rounded-md border px-3 py-2 text-sm {window.active
								? 'border-attention/40 bg-attention-soft'
								: ''}"
						>
							<span class="tabular text-muted-foreground">
								{new Date(window.starts_at).toLocaleString()} → {new Date(
									window.ends_at
								).toLocaleString()}
								{#if window.reason}
									<span class="block text-xs">{window.reason}</span>
								{/if}
							</span>
							<Badge variant={window.active ? 'down' : 'secondary'}>
								{window.active
									? 'suppressing now'
									: new Date(window.ends_at) < new Date()
										? 'ended'
										: 'scheduled'}
							</Badge>
						</div>
					{/each}
				</div>
				<p class="mt-2 text-xs text-muted-foreground">
					Checks still record during a window; an incident that would open is suppressed, and a
					target still broken at expiry opens on the next failing check.
				</p>
			</div>
		{/if}

		<div>
			<h2 class="text-sm font-medium text-muted-foreground">Recent checks</h2>
			<div class="mt-2">
				<CheckStrip checks={data.checks} intervalS={data.monitor.interval_s} />
			</div>
			{#if data.checks.length === 0}
				<p class="mt-2 text-sm text-muted-foreground">Nothing observed yet.</p>
			{:else}
				<div class="mt-2 flex flex-col gap-1.5">
					{#each data.checks as check (check.slot_at)}
						<div
							class="flex items-center justify-between gap-4 rounded-md border px-3 py-2 text-sm"
						>
							<span class="tabular text-muted-foreground">
								{new Date(check.slot_at).toLocaleTimeString()}
							</span>
							<span class="flex items-center gap-3">
								{#if check.latency_ms != null}
									<span class="tabular text-xs text-muted-foreground">{check.latency_ms}ms</span>
								{/if}
								<Badge variant={variantFor(check.outcome)}>{labelFor(check)}</Badge>
							</span>
						</div>
					{/each}
				</div>
			{/if}
		</div>

		<div>
			<h2 class="text-sm font-medium text-muted-foreground">Incidents</h2>
			{#if data.incidents.length === 0}
				<p class="mt-2 text-sm text-muted-foreground">No incidents recorded.</p>
			{:else}
				<div class="mt-2 flex flex-col gap-1.5">
					{#each data.incidents as incident (incident.id)}
						<div
							class="flex items-center justify-between gap-4 rounded-md border px-3 py-2 text-sm"
						>
							<span class="tabular text-muted-foreground">
								{new Date(incident.opened_at).toLocaleString()}
								{#if incident.first_fail_at}
									<span class="block text-xs">
										failing since {new Date(incident.first_fail_at).toLocaleTimeString()}
									</span>
								{/if}
							</span>
							<span class="text-muted-foreground">
								{incident.resolved_at
									? `resolved ${new Date(incident.resolved_at).toLocaleTimeString()}`
									: 'still open'}
							</span>
						</div>
					{/each}
				</div>
			{/if}
		</div>
	{/if}
</div>
