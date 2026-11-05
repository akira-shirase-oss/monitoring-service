<script lang="ts">
	import { Badge, type BadgeVariant } from '#lib/components/ui/badge/index.js';
	import * as Card from '#lib/components/ui/card/index.js';
	import { TriangleAlert } from '@lucide/svelte';
	import type { PageData } from './$types';
	import type { MonitorView } from './+page';

	let { data }: { data: PageData } = $props();

	function variantFor(status: MonitorView['status'], paused: boolean): BadgeVariant {
		if (paused) return 'secondary';
		switch (status) {
			case 'up':
				return 'up';
			case 'down':
				return 'down';
			default:
				return 'unknown';
		}
	}
</script>

<svelte:head><title>Monitors · Service Monitor</title></svelte:head>

<div class="flex flex-col gap-6">
	<div>
		<h1 class="text-2xl font-semibold tracking-tight">Monitors</h1>
		<p class="mt-1 text-sm text-muted-foreground">
			Authorized HTTP targets, checked on a schedule. Status here is evidence, not hope.
		</p>
	</div>

	{#if !data.reachable}
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
	{:else if data.monitors.length === 0}
		<Card.Root class="border-dashed">
			<Card.Content class="p-10 text-center">
				<p class="font-medium">No monitors yet.</p>
				<p class="mt-1 text-sm text-muted-foreground">
					Register the first one:
					<code class="rounded bg-muted px-1.5 py-0.5 font-mono text-xs">
						monitor monitors add &lt;name&gt; &lt;health-url&gt;
					</code>
				</p>
			</Card.Content>
		</Card.Root>
	{:else}
		<div class="flex flex-col gap-3">
			{#each data.monitors as monitor (monitor.id)}
				<Card.Root>
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
							<span class="tabular text-sm text-muted-foreground">every {monitor.interval_s}s</span>
							<Badge variant={variantFor(monitor.status, monitor.paused)}>
								{monitor.paused ? 'paused' : monitor.status}
							</Badge>
						</div>
					</Card.Content>
				</Card.Root>
			{/each}
		</div>
	{/if}
</div>
