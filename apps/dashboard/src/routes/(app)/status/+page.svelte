<script lang="ts">
	import { Badge, type BadgeVariant } from '#lib/components/ui/badge/index.js';
	import * as Card from '#lib/components/ui/card/index.js';
	import { ArrowLeft, TriangleAlert } from '@lucide/svelte';
	import { onMount } from 'svelte';
	import { invalidateAll } from '$app/navigation';
	import type { PageData } from './$types';

	let { data }: { data: PageData } = $props();

	// The snapshot is a heartbeat — poll it on the writer's cadence so the banner
	// flips the moment a beat goes stale. Same atomic swap as the rest of the app.
	onMount(() => {
		const timer = setInterval(() => {
			if (document.visibilityState === 'visible') invalidateAll();
		}, 15_000);
		return () => clearInterval(timer);
	});

	function variantFor(status: string): BadgeVariant {
		switch (status) {
			case 'up':
				return 'live';
			case 'down':
				return 'attention';
			default:
				return 'draft';
		}
	}

	function age(ts: string): string {
		return `${Math.max(0, Math.round((Date.now() - Date.parse(ts)) / 1000))}s ago`;
	}
</script>

<svelte:head><title>Service status · Service Monitor</title></svelte:head>

<div class="flex flex-col gap-6">
	<a
		href="/"
		class="inline-flex w-fit items-center gap-1.5 text-sm text-muted-foreground hover:text-foreground"
	>
		<ArrowLeft class="size-4" /> Monitors
	</a>
	<div>
		<h1 class="text-2xl font-semibold tracking-tight">Service status</h1>
		<p class="mt-1 text-sm text-muted-foreground">
			The public snapshot — hashed aliases and committed states only.
		</p>
	</div>

	{#if data.state === 'unavailable'}
		<Card.Root class="border-attention/40 bg-attention-soft">
			<Card.Content class="flex items-start gap-3 p-4">
				<TriangleAlert class="mt-0.5 size-4 shrink-0 text-attention-ink" />
				<div>
					<p class="text-sm font-medium text-attention-ink">Monitoring unavailable.</p>
					<p class="mt-0.5 text-sm text-attention-ink/80">
						No status snapshot could be read — this says nothing about your targets.
					</p>
				</div>
			</Card.Content>
		</Card.Root>
	{:else}
		{#if data.state === 'stale'}
			<Card.Root class="border-attention/40 bg-attention-soft">
				<Card.Content class="flex items-start gap-3 p-4">
					<TriangleAlert class="mt-0.5 size-4 shrink-0 text-attention-ink" />
					<div>
						<p class="text-sm font-medium text-attention-ink">
							Monitoring unavailable — this snapshot is stale.
						</p>
						<p class="mt-0.5 text-sm text-attention-ink/80">
							The service states below were last observed {age(data.snap.generated_at)}.
						</p>
					</div>
				</Card.Content>
			</Card.Root>
		{/if}

		{#if data.snap.services.length === 0}
			<Card.Root class="border-dashed">
				<Card.Content class="p-10 text-center">
					<p class="text-sm text-muted-foreground">No services in this snapshot.</p>
				</Card.Content>
			</Card.Root>
		{:else}
			<div class="flex flex-col gap-2">
				{#each data.snap.services as service (service.alias)}
					<div class="flex items-center justify-between gap-4 rounded-md border px-3 py-2 text-sm">
						<span class="font-mono text-xs text-muted-foreground">{service.alias}</span>
						<span class="flex items-center gap-3">
							{#if service.incident_open_since}
								<span class="text-xs text-muted-foreground">
									incident since {new Date(service.incident_open_since).toLocaleString()}
								</span>
							{/if}
							<Badge variant={variantFor(service.status)}>{service.status}</Badge>
						</span>
					</div>
				{/each}
			</div>
		{/if}
		<p class="text-xs text-muted-foreground">
			snapshot written {age(data.snap.generated_at)}{data.state === 'stale' ? ' — expired' : ''}
		</p>
	{/if}
</div>
