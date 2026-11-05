<script lang="ts">
	import { Badge, type BadgeVariant } from '#lib/components/ui/badge/index.js';
	import * as Card from '#lib/components/ui/card/index.js';
	import { ArrowLeft, TriangleAlert } from '@lucide/svelte';
	import type { PageData } from './$types';
	import type { CheckView } from './+page';

	let { data }: { data: PageData } = $props();

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

	{#if data.state === 'missing'}
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
				every {data.monitor.interval_s}s · deadline {data.monitor.timeout_s}s · generation {data
					.monitor.generation}
			</p>
		</div>

		<div>
			<h2 class="text-sm font-medium text-muted-foreground">Recent checks</h2>
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
