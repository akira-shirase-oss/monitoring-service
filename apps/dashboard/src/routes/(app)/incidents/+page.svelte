<script lang="ts">
	import SirenIcon from '@lucide/svelte/icons/siren';
	import { onMount } from 'svelte';
	import { invalidateAll } from '$app/navigation';
	import { Badge } from '#lib/components/ui/badge/index.js';
	import * as Card from '#lib/components/ui/card/index.js';
	import * as Table from '#lib/components/ui/table/index.js';
	import EmptyState from '#lib/components/shell/EmptyState.svelte';
	import PageHeader from '#lib/components/shell/PageHeader.svelte';
	import Problem from '#lib/components/shell/Problem.svelte';
	import { shown } from '#lib/call.js';
	import { span, when } from '#lib/time.js';
	import type { PageData } from './$types';
	import type { IncidentView } from './+page';

	let { data }: { data: PageData } = $props();

	// Same honest poll as the monitors list: atomic swap, hidden tabs don't poll.
	let now = $state(Date.now());
	onMount(() => {
		const timer = setInterval(() => {
			now = Date.now();
			if (document.visibilityState === 'visible') invalidateAll();
		}, 15_000);
		return () => clearInterval(timer);
	});

	const open = $derived(data.incidents.filter((i) => i.resolved_at === null));
	const closed = $derived(data.incidents.filter((i) => i.resolved_at !== null));

	// An incident starts at its first failing check, which is earlier than the moment it
	// opened (it opens only after the failure threshold). Older rows don't carry it.
	function began(incident: IncidentView): string {
		return incident.first_fail_at ?? incident.opened_at;
	}

	function lasted(incident: IncidentView): string {
		const end = incident.resolved_at ? Date.parse(incident.resolved_at) : now;
		return span(end - Date.parse(began(incident)));
	}
</script>

<svelte:head><title>Incidents · Service Monitor</title></svelte:head>

<PageHeader title="Incidents">
	{#snippet description()}
		An incident opens after a monitor fails several checks in a row and closes after it passes
		again. Missed checks never close one.
	{/snippet}
</PageHeader>

{#snippet rows(list: IncidentView[], isOpen: boolean)}
	<Card.Root>
		<Card.Content class="p-0">
			<Table.Root>
				<Table.Header>
					<Table.Row>
						<Table.Head>Monitor</Table.Head>
						<Table.Head>First failure</Table.Head>
						<Table.Head>{isOpen ? 'Last failure' : 'Recovered'}</Table.Head>
						<Table.Head class="text-right">{isOpen ? 'Open for' : 'Lasted'}</Table.Head>
					</Table.Row>
				</Table.Header>
				<Table.Body>
					{#each list as incident (incident.id)}
						<Table.Row data-testid="incident-row" data-state={isOpen ? 'open' : 'resolved'}>
							<Table.Cell class="font-medium">
								{#if data.names[incident.monitor_id]}
									<a class="hover:underline" href="/monitors/{incident.monitor_id}">{data.names[incident.monitor_id]}</a>
								{:else}
									<span class="text-muted-foreground">Deleted monitor</span>
								{/if}
							</Table.Cell>
							<Table.Cell class="text-muted-foreground">{when(began(incident))}</Table.Cell>
							<Table.Cell class="text-muted-foreground">
								{when(isOpen ? incident.last_fail_at : incident.resolved_at)}
							</Table.Cell>
							<Table.Cell class="tabular text-right">{lasted(incident)}</Table.Cell>
						</Table.Row>
					{/each}
				</Table.Body>
			</Table.Root>
		</Card.Content>
	</Card.Root>
{/snippet}

<div class="flex flex-col gap-8">
	{#if data.failure}
		<Problem error={shown(data.failure)} what="the incidents" retry={invalidateAll} />
	{:else if data.incidents.length === 0}
		<EmptyState icon={SirenIcon} title="No incidents recorded" testid="empty-incidents">
			Nothing has failed often enough to open one. That is a statement about what was checked,
			not a promise that everything is healthy.
		</EmptyState>
	{:else}
		<section class="flex flex-col gap-3" aria-labelledby="open-heading">
			<h2 id="open-heading" class="flex items-center gap-2 text-sm font-medium">
				Open <Badge variant={open.length > 0 ? 'attention' : 'draft'} data-testid="open-count">{open.length}</Badge>
			</h2>
			{#if open.length === 0}
				<p class="text-sm text-muted-foreground">No incident is open right now.</p>
			{:else}
				{@render rows(open, true)}
			{/if}
		</section>
		{#if closed.length > 0}
			<section class="flex flex-col gap-3" aria-labelledby="closed-heading">
				<h2 id="closed-heading" class="text-sm font-medium">Recovered</h2>
				{@render rows(closed, false)}
			</section>
		{/if}
	{/if}
</div>
