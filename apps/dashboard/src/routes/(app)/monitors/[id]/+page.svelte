<script lang="ts">
	import { Badge, type BadgeVariant } from '#lib/components/ui/badge/index.js';
	import * as Card from '#lib/components/ui/card/index.js';
	import { ArrowLeft, TriangleAlert } from '@lucide/svelte';
	import { onMount } from 'svelte';
	import { goto, invalidateAll } from '$app/navigation';
	import Confirm from '#lib/components/shell/Confirm.svelte';
	import Problem from '#lib/components/shell/Problem.svelte';
	import MonitorBadge from '#lib/components/shell/MonitorBadge.svelte';
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
	let editGeneration = $state(0);
	let confirmingDelete = $state(false);
	let actionError = $state<string | null>(null);
	let actionBusy = $state(false);

	let addingWindow = $state(false);
	let windowStart = $state('');
	let windowEnd = $state('');
	let windowReason = $state('');
	let confirmingWindow = $state<number | null>(null);
	let windowError = $state<string | null>(null);
	let windowBusy = $state(false);

	// Every management write shows the server's own error — a 409 conflict is real
	// information (stale read, or the monitor has history), never something to swallow.
	// Returns the failure text (null on success) so each section shows it near its form.
	async function write(path: string, init: RequestInit): Promise<string | null> {
		try {
			const response = await apiFetch(path, init, fetch);
			const failure = await readEnvelope(response);
			return failure ? failure.message : null;
		} catch {
			return 'The monitor server is unreachable.';
		}
	}

	async function mutate(id: string, init: RequestInit): Promise<boolean> {
		actionBusy = true;
		try {
			actionError = await write(`/api/monitors/${id}`, init);
			return actionError === null;
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

	// Deleting takes the monitor's recorded history with it — checks, incidents, queued
	// alerts, maintenance windows — so it asks for exactly that, after a confirmation
	// that names what goes. The dialog stays open on a refusal and shows the reason.
	async function remove(id: string) {
		if (await mutate(`${id}?evidence=delete`, { method: 'DELETE' })) {
			confirmingDelete = false;
			await goto('/');
		}
	}

	// A window's start is optional in the form — empty means "from the next slot".
	// The API wants explicit RFC 3339 bounds, so "now" is filled in here.
	async function addWindow(id: string) {
		const start = windowStart ? new Date(windowStart) : new Date();
		const end = new Date(windowEnd);
		if (Number.isNaN(start.getTime()) || Number.isNaN(end.getTime())) {
			windowError = 'An end time is required.';
			return;
		}
		windowBusy = true;
		try {
			windowError = await write(`/api/monitors/${id}/maintenance`, {
				method: 'POST',
				headers: { 'content-type': 'application/json' },
				body: JSON.stringify({
					starts_at: start.toISOString(),
					ends_at: end.toISOString(),
					...(windowReason.trim() ? { reason: windowReason.trim() } : {})
				})
			});
			if (windowError === null) {
				addingWindow = false;
				windowStart = windowEnd = windowReason = '';
				await invalidateAll();
			}
		} finally {
			windowBusy = false;
		}
	}

	// Same two-click confirm as monitor delete — cancelling an active window resumes
	// paging on the next slot, so the second click names what it does.
	async function removeWindow(windowId: number) {
		if (confirmingWindow !== windowId) {
			confirmingWindow = windowId;
			return;
		}
		confirmingWindow = null;
		windowBusy = true;
		try {
			windowError = await write(`/api/maintenance/${windowId}`, { method: 'DELETE' });
			if (windowError === null) await invalidateAll();
		} finally {
			windowBusy = false;
		}
	}

	function variantFor(outcome: CheckView['outcome']): BadgeVariant {
		switch (outcome) {
			case 'pass':
				return 'live';
			case 'missed':
				return 'draft';
			default:
				return 'attention';
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
		<Problem error={{ status: 401, message: 'Your session ended — sign in again.' }} what="the monitor" />
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
				<MonitorBadge status={data.monitor.status} paused={data.monitor.paused} />
			</div>
			<p class="mt-1 truncate font-mono text-sm text-muted-foreground">{data.monitor.url}</p>
			<p class="mt-0.5 text-sm text-muted-foreground">
				every {data.monitor.interval_s}s · deadline {data.monitor.timeout_s}s · opens after {data
					.monitor.failures_to_open} fails · recovers after {data.monitor.passes_to_recover} passes
				· generation {data.monitor.generation}
			</p>
			<p class="mt-0.5 text-sm text-muted-foreground" data-testid="monitor-expectations">
				{data.monitor.method} request · passes on status {data.monitor.expect_statuses}{#if data.monitor.expect_body}
					· body must contain “{data.monitor.expect_body}”{/if}
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
						// The guard is the generation the person was looking at when they
						// opened the form, not whatever a later poll brought in: an edit
						// made elsewhere meanwhile must come back as a conflict.
						editGeneration = data.monitor.generation;
						editing = !editing;
						actionError = null;
					}}
					disabled={actionBusy}
					class="rounded-md border border-input px-3 py-1.5 text-sm disabled:opacity-50"
				>
					{editing ? 'Close edit' : 'Edit'}
				</button>
				<button
					onclick={() => {
						actionError = null;
						confirmingDelete = true;
					}}
					disabled={actionBusy}
					class="rounded-md border border-attention/40 px-3 py-1.5 text-sm text-attention-ink disabled:opacity-50"
					data-testid="monitor-delete"
				>
					Delete
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
								name: data.monitor.name,
								url: data.monitor.url,
								interval_s: data.monitor.interval_s,
								timeout_s: data.monitor.timeout_s,
								failures_to_open: data.monitor.failures_to_open,
								passes_to_recover: data.monitor.passes_to_recover,
								method: data.monitor.method,
								expect_statuses: data.monitor.expect_statuses,
								expect_body: data.monitor.expect_body
							}}
							showIdentity
							submitLabel="Save changes"
							onSubmit={(payload) => saveEdit(data.monitor.id, editGeneration, payload)}
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

		<div>
			<div class="flex items-center justify-between gap-4">
				<h2 class="text-sm font-medium text-muted-foreground">Maintenance windows</h2>
				<button
					onclick={() => {
						addingWindow = !addingWindow;
						windowError = null;
					}}
					disabled={windowBusy}
					class="rounded-md border border-input px-3 py-1.5 text-sm disabled:opacity-50"
				>
					{addingWindow ? 'Close' : 'Add window'}
				</button>
			</div>
			{#if addingWindow}
				<form
					class="mt-2 grid grid-cols-2 gap-3 sm:grid-cols-3"
					onsubmit={(event) => {
						event.preventDefault();
						addWindow(data.monitor.id);
					}}
				>
					<div>
						<span class="block text-xs font-medium text-muted-foreground">from (empty = now)</span>
						<input
							bind:value={windowStart}
							type="datetime-local"
							class="w-full rounded-md border border-input bg-background px-3 py-1.5 text-sm tabular"
						/>
					</div>
					<div>
						<span class="block text-xs font-medium text-muted-foreground">until</span>
						<input
							bind:value={windowEnd}
							required
							type="datetime-local"
							class="w-full rounded-md border border-input bg-background px-3 py-1.5 text-sm tabular"
						/>
					</div>
					<div>
						<span class="block text-xs font-medium text-muted-foreground">reason</span>
						<input
							bind:value={windowReason}
							maxlength={200}
							class="w-full rounded-md border border-input bg-background px-3 py-1.5 text-sm"
							placeholder="deploy rehearsal"
						/>
					</div>
					<div class="col-span-2 flex items-end justify-end sm:col-span-3">
						<button
							type="submit"
							disabled={windowBusy}
							class="rounded-md bg-primary px-3 py-1.5 text-sm font-medium text-primary-foreground disabled:opacity-50"
						>
							{windowBusy ? 'Saving…' : 'Add window'}
						</button>
					</div>
				</form>
			{/if}
			{#if windowError}
				<p class="mt-2 text-sm text-attention-ink">{windowError}</p>
			{/if}
			{#if data.windows.length === 0 && !addingWindow}
				<p class="mt-2 text-sm text-muted-foreground">
					None set — a window suppresses incident alerts while checks keep recording.
				</p>
			{:else if data.windows.length > 0}
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
							<span class="flex shrink-0 items-center gap-3">
								<Badge variant={window.active ? 'pending' : 'secondary'}>
									{window.active
										? 'suppressing now'
										: new Date(window.ends_at) < new Date()
											? 'ended'
											: 'scheduled'}
								</Badge>
								<button
									onclick={() => removeWindow(window.id)}
									disabled={windowBusy}
									class="rounded-md border border-input px-2 py-1 text-xs text-muted-foreground hover:text-foreground disabled:opacity-50"
								>
									{confirmingWindow === window.id ? 'Confirm cancel' : 'Cancel'}
								</button>
							</span>
						</div>
					{/each}
				</div>
			{/if}
			<p class="mt-2 text-xs text-muted-foreground">
				Checks still record during a window; an incident that would open is suppressed, and a
				target still broken at expiry opens on the next failing check.
			</p>
		</div>

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


{#if data.state === 'ok'}
	<Confirm
		bind:open={confirmingDelete}
		title="Delete {data.monitor.name}?"
		confirm="Delete monitor and history"
		busy={actionBusy}
		onConfirm={() => remove(data.monitor.id)}
	>
		Its recorded checks, incidents, queued alerts and maintenance windows are deleted with it,
		and it leaves the public status page. This can't be undone. To stop checking but keep the
		history, pause it instead.
		{#if actionError}<span class="mt-2 block text-attention-ink" role="alert">{actionError}</span>{/if}
	</Confirm>
{/if}
