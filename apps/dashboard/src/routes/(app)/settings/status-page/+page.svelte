<script lang="ts">
	import ExternalLinkIcon from '@lucide/svelte/icons/external-link';
	import { untrack } from 'svelte';
	import { invalidateAll } from '$app/navigation';
	import { Badge } from '#lib/components/ui/badge/index.js';
	import { Button } from '#lib/components/ui/button/index.js';
	import * as Card from '#lib/components/ui/card/index.js';
	import { Checkbox } from '#lib/components/ui/checkbox/index.js';
	import { Input } from '#lib/components/ui/input/index.js';
	import FormRow from '#lib/components/shell/FormRow.svelte';
	import PageHeader from '#lib/components/shell/PageHeader.svelte';
	import Problem from '#lib/components/shell/Problem.svelte';
	import { call, shown } from '#lib/call.js';
	import { current } from '#lib/workspace.svelte.js';
	import type { PageData } from './$types';
	import type { StatusPageView } from './+page';

	let { data }: { data: PageData } = $props();

	const canWrite = $derived(current()?.role !== 'viewer');

	// The form starts on what is stored and then belongs to the person editing it.
	let title = $state(untrack(() => data.page?.title ?? ''));
	let slug = $state(untrack(() => data.page?.slug ?? ''));
	let chosen = $state<string[]>(untrack(() => [...(data.page?.monitor_ids ?? [])]));
	let busy = $state(false);
	let error = $state<string | null>(null);
	let saved = $state(false);

	// The page is public only while it has a title; the stored row is the truth about
	// that, not the half-typed field.
	const live = $derived((data.page?.title ?? '') !== '');
	const address = $derived(`${location.origin}/s/${data.page?.slug ?? ''}`);

	function toggle(id: string, on: boolean) {
		chosen = on ? [...chosen.filter((x) => x !== id), id] : chosen.filter((x) => x !== id);
		saved = false;
	}

	async function save(event: SubmitEvent) {
		event.preventDefault();
		busy = true;
		error = null;
		saved = false;
		const trimmed = title.trim();
		const answer = await call<{ status_page: StatusPageView }>('/api/workspace/status-page', {
			method: 'PUT',
			body: { slug: slug.trim(), title: trimmed === '' ? null : trimmed, monitor_ids: chosen }
		});
		busy = false;
		if (!answer.ok) {
			error = answer.failure.message;
			return;
		}
		slug = answer.body.status_page.slug;
		saved = true;
		await invalidateAll();
	}
</script>

<svelte:head><title>Status page · Service Monitor</title></svelte:head>

<PageHeader title="Status page">
	{#snippet description()}
		A public page for the people who depend on your services. It shows only the monitors you
		pick, by name, and never their URLs.
	{/snippet}
	{#snippet meta()}
		{#if data.page}
			{#if live}
				<Badge variant="live" data-testid="status-page-state">Public</Badge>
				<a class="inline-flex items-center gap-1 underline" href="/s/{data.page.slug}" target="_blank" rel="noopener" data-testid="status-page-link">
					{address} <ExternalLinkIcon class="size-3.5" aria-hidden="true" />
				</a>
			{:else}
				<Badge variant="draft" data-testid="status-page-state">Off</Badge>
				<span>Give the page a title to publish it.</span>
			{/if}
		{/if}
	{/snippet}
</PageHeader>

<div class="flex flex-col gap-6">
	{#if data.failure || !data.page}
		<Problem error={data.failure ? shown(data.failure) : null} what="the status page" retry={invalidateAll} />
	{:else}
		<Card.Root>
			<Card.Content>
				<form onsubmit={save}>
					<FormRow
						label="Title"
						hint="Shown at the top of the page. Clear it to take the page offline; your choices below are kept."
						for="status-title"
					>
						<Input
							id="status-title"
							bind:value={title}
							maxlength={120}
							readonly={!canWrite}
							placeholder="Acme status"
							data-testid="status-title"
							oninput={() => (saved = false)}
						/>
					</FormRow>
					<FormRow
						label="Address"
						hint="Lowercase letters, digits and hyphens. Changing it breaks links to the old address."
						for="status-slug"
					>
						<div class="flex items-center gap-2">
							<span class="shrink-0 font-mono text-sm text-muted-foreground">/s/</span>
							<Input
								id="status-slug"
								bind:value={slug}
								required
								maxlength={64}
								pattern="[a-z0-9-]+"
								readonly={!canWrite}
								class="font-mono text-sm"
								data-testid="status-slug"
								oninput={() => (saved = false)}
							/>
						</div>
					</FormRow>
					<FormRow label="Monitors" hint="Shown in the order you tick them.">
						{#if data.monitors.length === 0}
							<p class="text-sm text-muted-foreground">
								No monitors yet. <a class="underline" href="/">Add one</a>, then choose it here.
							</p>
						{:else}
							<ul class="flex flex-col gap-3" data-testid="status-monitors">
								{#each data.monitors as monitor (monitor.id)}
									<li class="flex items-start gap-3">
										<Checkbox
											id="pick-{monitor.id}"
											checked={chosen.includes(monitor.id)}
											disabled={!canWrite}
											onCheckedChange={(on: boolean) => toggle(monitor.id, on)}
										/>
										<label for="pick-{monitor.id}" class="min-w-0 text-sm leading-tight">
											<span class="font-medium">{monitor.name}</span>
											<span class="block truncate font-mono text-xs text-muted-foreground">{monitor.url}</span>
										</label>
									</li>
								{/each}
							</ul>
						{/if}
					</FormRow>
					{#if canWrite}
						<div class="flex flex-wrap items-center gap-3 pt-5">
							<Button type="submit" disabled={busy} data-testid="status-save">
								{busy ? 'Saving…' : 'Save'}
							</Button>
							{#if error}
								<p class="text-sm text-attention-ink" role="alert" data-testid="status-error">{error}</p>
							{:else if saved}
								<p class="text-sm text-muted-foreground" role="status" data-testid="status-saved">Saved.</p>
							{/if}
						</div>
					{/if}
				</form>
			</Card.Content>
		</Card.Root>
	{/if}
</div>
