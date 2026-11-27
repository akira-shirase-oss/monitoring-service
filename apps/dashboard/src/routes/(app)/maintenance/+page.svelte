<script lang="ts">
	import CalendarClockIcon from '@lucide/svelte/icons/calendar-clock';
	import { invalidateAll } from '$app/navigation';
	import { Badge } from '#lib/components/ui/badge/index.js';
	import { Button } from '#lib/components/ui/button/index.js';
	import * as Card from '#lib/components/ui/card/index.js';
	import { Input } from '#lib/components/ui/input/index.js';
	import { NativeSelect, NativeSelectOption } from '#lib/components/ui/native-select/index.js';
	import * as Table from '#lib/components/ui/table/index.js';
	import Confirm from '#lib/components/shell/Confirm.svelte';
	import EmptyState from '#lib/components/shell/EmptyState.svelte';
	import PageHeader from '#lib/components/shell/PageHeader.svelte';
	import Problem from '#lib/components/shell/Problem.svelte';
	import { call, shown } from '#lib/call.js';
	import { fromLocalInput, when } from '#lib/time.js';
	import { current } from '#lib/workspace.svelte.js';
	import type { PageData } from './$types';
	import type { WindowView } from './+page';

	let { data }: { data: PageData } = $props();

	const canWrite = $derived(current()?.role !== 'viewer');
	const names = $derived(Object.fromEntries(data.monitors.map((m) => [m.id, m.name])));
	// Finished windows stay as a record of what was suppressed; they sort last.
	const upcoming = $derived(data.windows.filter((w) => Date.parse(w.ends_at) >= Date.now()));
	const past = $derived(data.windows.filter((w) => Date.parse(w.ends_at) < Date.now()).reverse());

	let adding = $state(false);
	let monitorId = $state('');
	let starts = $state('');
	let ends = $state('');
	let reason = $state('');
	let busy = $state(false);
	let formError = $state<string | null>(null);

	let cancelling = $state<WindowView | null>(null);
	let cancelOpen = $state(false);
	let cancelBusy = $state(false);
	let cancelError = $state<string | null>(null);

	function open() {
		monitorId = data.monitors[0]?.id ?? '';
		starts = '';
		ends = '';
		reason = '';
		formError = null;
		adding = true;
	}

	async function add(event: SubmitEvent) {
		event.preventDefault();
		const startsAt = fromLocalInput(starts);
		const endsAt = fromLocalInput(ends);
		if (startsAt === null || endsAt === null) {
			formError = 'Pick a start and an end.';
			return;
		}
		busy = true;
		formError = null;
		const trimmed = reason.trim();
		const answer = await call(`/api/monitors/${monitorId}/maintenance`, {
			method: 'POST',
			body: { starts_at: startsAt, ends_at: endsAt, ...(trimmed === '' ? {} : { reason: trimmed }) }
		});
		busy = false;
		if (!answer.ok) {
			formError = answer.failure.message;
			return;
		}
		adding = false;
		await invalidateAll();
	}

	async function cancel() {
		if (cancelling === null) return;
		cancelBusy = true;
		cancelError = null;
		const answer = await call(`/api/maintenance/${cancelling.id}`, { method: 'DELETE' });
		cancelBusy = false;
		if (!answer.ok) {
			cancelError = answer.failure.message;
			return;
		}
		cancelOpen = false;
		cancelling = null;
		await invalidateAll();
	}
</script>

<svelte:head><title>Maintenance · Service Monitor</title></svelte:head>

<PageHeader title="Maintenance">
	{#snippet description()}
		During a window, checks keep running and are still recorded, but a failing monitor doesn't
		open an incident or send an alert.
	{/snippet}
	{#snippet actions()}
		{#if canWrite && !adding && !data.failure && data.monitors.length > 0}
			<Button size="sm" onclick={open} data-testid="new-window">New window</Button>
		{/if}
	{/snippet}
</PageHeader>

{#snippet rows(list: WindowView[], live: boolean)}
	<Card.Root>
		<Card.Content class="p-0">
			<Table.Root>
				<Table.Header>
					<Table.Row>
						<Table.Head>Monitor</Table.Head>
						<Table.Head>Starts</Table.Head>
						<Table.Head class="hidden sm:table-cell">Ends</Table.Head>
						<Table.Head class="hidden sm:table-cell">Reason</Table.Head>
						<Table.Head class="text-right"><span class="sr-only">State</span></Table.Head>
					</Table.Row>
				</Table.Header>
				<Table.Body>
					{#each list as window (window.id)}
						<Table.Row data-testid="window-row">
							<Table.Cell class="whitespace-normal font-medium sm:whitespace-nowrap">
								<a class="hover:underline" href="/monitors/{window.monitor_id}">{names[window.monitor_id] ?? 'Monitor'}</a>
								{#if window.reason}<span class="block text-xs font-normal text-muted-foreground sm:hidden">{window.reason}</span>{/if}
							</Table.Cell>
							<Table.Cell class="whitespace-normal text-muted-foreground sm:whitespace-nowrap">
								{when(window.starts_at)}
								<span class="block sm:hidden">to {when(window.ends_at)}</span>
							</Table.Cell>
							<Table.Cell class="hidden text-muted-foreground sm:table-cell">{when(window.ends_at)}</Table.Cell>
							<Table.Cell class="hidden text-muted-foreground sm:table-cell">{window.reason ?? '—'}</Table.Cell>
							<Table.Cell class="text-right">
								<span class="inline-flex items-center gap-2">
									{#if window.active}<Badge variant="pending">Active now</Badge>{/if}
									{#if live && canWrite}
										<Button
											variant="outline"
											size="xs"
											data-testid="window-cancel"
											onclick={() => {
												cancelling = window;
												cancelError = null;
												cancelOpen = true;
											}}>{window.active ? 'End now' : 'Cancel'}</Button
										>
									{/if}
								</span>
							</Table.Cell>
						</Table.Row>
					{/each}
				</Table.Body>
			</Table.Root>
		</Card.Content>
	</Card.Root>
{/snippet}

<div class="flex flex-col gap-8">
	{#if data.failure}
		<Problem error={shown(data.failure)} what="the maintenance windows" retry={invalidateAll} />
	{:else}
		{#if data.unread.length > 0}
			<p class="text-sm text-attention-ink" role="alert">
				Couldn't read the windows of {data.unread.join(', ')}. The list below is incomplete.
			</p>
		{/if}

		{#if adding}
			<Card.Root>
				<Card.Header>
					<Card.Title>New maintenance window</Card.Title>
					<Card.Description>Times are in your browser's time zone.</Card.Description>
				</Card.Header>
				<Card.Content>
					<form class="grid gap-4 sm:grid-cols-2" onsubmit={add}>
						<div class="space-y-1.5 sm:col-span-2">
							<label for="window-monitor" class="text-sm font-medium">Monitor</label>
							<NativeSelect id="window-monitor" bind:value={monitorId} class="w-full" data-testid="window-monitor">
								{#each data.monitors as monitor (monitor.id)}
									<NativeSelectOption value={monitor.id}>{monitor.name}</NativeSelectOption>
								{/each}
							</NativeSelect>
						</div>
						<div class="space-y-1.5">
							<label for="window-starts" class="text-sm font-medium">Starts</label>
							<Input id="window-starts" type="datetime-local" bind:value={starts} required data-testid="window-starts" />
						</div>
						<div class="space-y-1.5">
							<label for="window-ends" class="text-sm font-medium">Ends</label>
							<Input id="window-ends" type="datetime-local" bind:value={ends} required data-testid="window-ends" />
						</div>
						<div class="space-y-1.5 sm:col-span-2">
							<label for="window-reason" class="text-sm font-medium">Reason <span class="font-normal text-muted-foreground">(optional)</span></label>
							<Input id="window-reason" bind:value={reason} maxlength={200} placeholder="Database upgrade" data-testid="window-reason" />
						</div>
						<div class="flex items-center gap-2 sm:col-span-2">
							<Button type="submit" disabled={busy} data-testid="window-create">
								{busy ? 'Scheduling…' : 'Schedule window'}
							</Button>
							<Button type="button" variant="outline" onclick={() => (adding = false)}>Cancel</Button>
							{#if formError}
								<p class="text-sm text-attention-ink" role="alert" data-testid="window-error">{formError}</p>
							{/if}
						</div>
					</form>
				</Card.Content>
			</Card.Root>
		{/if}

		{#if data.monitors.length === 0}
			<EmptyState icon={CalendarClockIcon} title="No monitors to schedule for" testid="empty-maintenance">
				A maintenance window belongs to a monitor. <a class="underline" href="/">Add a monitor</a> first.
			</EmptyState>
		{:else if data.windows.length === 0 && !adding}
			<EmptyState icon={CalendarClockIcon} title="No windows scheduled" testid="empty-maintenance">
				Schedule one before planned work so a known outage doesn't page anyone.
				{#snippet action()}
					{#if canWrite}<Button size="sm" onclick={open}>Schedule a window</Button>{/if}
				{/snippet}
			</EmptyState>
		{:else}
			{#if upcoming.length > 0}
				<section class="flex flex-col gap-3" aria-labelledby="upcoming-heading">
					<h2 id="upcoming-heading" class="text-sm font-medium">Active and upcoming</h2>
					{@render rows(upcoming, true)}
				</section>
			{/if}
			{#if past.length > 0}
				<section class="flex flex-col gap-3" aria-labelledby="past-heading">
					<h2 id="past-heading" class="text-sm font-medium">Finished</h2>
					{@render rows(past, false)}
				</section>
			{/if}
		{/if}
	{/if}
</div>

<Confirm
	bind:open={cancelOpen}
	title={cancelling?.active ? 'End this window now?' : 'Cancel this window?'}
	confirm={cancelling?.active ? 'End window' : 'Cancel window'}
	busy={cancelBusy}
	onConfirm={cancel}
>
	{names[cancelling?.monitor_id ?? ''] ?? 'The monitor'} goes back to opening incidents and sending
	alerts when it fails.
	{#if cancelError}<span class="mt-2 block text-attention-ink" role="alert">{cancelError}</span>{/if}
</Confirm>
