<!--
  A workspace's public status page. It reads one small JSON file and says exactly what
  that file can support: a service is "up" only when its latest checks passed, a service
  nobody has checked is "unknown", and when the monitor itself has stopped writing the
  page says so instead of repeating the last good news.
-->
<script lang="ts">
	import { onMount } from 'svelte';
	import { invalidateAll } from '$app/navigation';
	import { Badge, type BadgeVariant } from '#lib/components/ui/badge/index.js';
	import ThemeToggle from '#lib/components/shell/ThemeToggle.svelte';
	import { ago, percent, when } from '#lib/time.js';
	import type { PageData } from './$types';
	import type { PublicService } from './+page';

	let { data }: { data: PageData } = $props();

	let now = $state(Date.now());
	onMount(() => {
		const timer = setInterval(() => {
			now = Date.now();
			if (document.visibilityState === 'visible') invalidateAll();
		}, 30_000);
		return () => clearInterval(timer);
	});

	const services = $derived(data.snap?.services ?? []);
	const down = $derived(services.filter((s) => s.status === 'down').length);
	const unknown = $derived(services.filter((s) => s.status === 'unknown').length);

	type Verdict = { tone: 'live' | 'attention' | 'draft'; line: string; detail: string };
	const verdict = $derived.by<Verdict>(() => {
		if (data.state === 'unreachable') {
			return {
				tone: 'draft',
				line: 'Status unavailable',
				detail: "The status service didn't answer. This says nothing about the services themselves."
			};
		}
		if (data.state === 'stale') {
			return {
				tone: 'draft',
				line: 'Monitoring unavailable',
				detail: 'The monitor has stopped reporting, so current status is unknown. What you see below is the last recorded state.'
			};
		}
		if (services.length === 0) {
			return { tone: 'draft', line: 'Nothing published yet', detail: 'No services are listed on this page.' };
		}
		if (down > 0) {
			return {
				tone: 'attention',
				line: down === 1 ? '1 service is down' : `${down} services are down`,
				detail: 'An incident is open. It closes after the service passes its checks again.'
			};
		}
		if (unknown > 0) {
			return {
				tone: 'draft',
				line: 'Some services have no recent result',
				detail: 'A service without a recent check is shown as unknown, never as up.'
			};
		}
		return {
			tone: 'live',
			line: 'All listed services are up',
			detail: 'Every service below passed its most recent checks.'
		};
	});

	function variantFor(service: PublicService): BadgeVariant {
		if (data.state === 'stale') return 'draft';
		return service.status === 'up' ? 'live' : service.status === 'down' ? 'attention' : 'draft';
	}

	const title = $derived(data.snap?.title ?? 'Service status');
</script>

<svelte:head>
	<title>{data.state === 'absent' ? 'No status page here' : title}</title>
	<meta name="robots" content="noindex" />
</svelte:head>

<div class="flex min-h-svh flex-col bg-background">
	<main class="mx-auto w-full max-w-2xl flex-1 px-4 py-10 sm:py-16">
		{#if data.state === 'absent'}
			<div data-testid="public-absent">
				<h1 class="page-title">No status page here</h1>
				<p class="page-intro mt-2">
					This address doesn't belong to a published status page. Check the link, or ask whoever
					shared it.
				</p>
			</div>
		{:else}
			<header class="flex items-start justify-between gap-4">
				<h1 class="page-title" data-testid="public-title">{title}</h1>
				<ThemeToggle />
			</header>

			<section
				class="mt-8 rounded-card border p-5 {verdict.tone === 'live'
					? 'border-live/40 bg-live-soft'
					: verdict.tone === 'attention'
						? 'border-attention/40 bg-attention-soft'
						: 'border-border bg-card'}"
				aria-live="polite"
				data-testid="public-verdict"
				data-tone={verdict.tone}
			>
				<p
					class="font-display text-lg {verdict.tone === 'live'
						? 'text-live-ink'
						: verdict.tone === 'attention'
							? 'text-attention-ink'
							: ''}"
				>
					{verdict.line}
				</p>
				<p class="mt-1 text-sm text-muted-foreground">{verdict.detail}</p>
			</section>

			{#if services.length > 0}
				<ul class="mt-6 flex flex-col divide-y divide-border rounded-card border border-border bg-card" data-testid="public-services">
					<!-- By position: names are not unique, and a keyed list with a repeated key
					     stops rendering — two monitors given one name must not take the page down. -->
					{#each services as service, i (i)}
						<li class="flex flex-wrap items-center justify-between gap-x-4 gap-y-2 px-5 py-4" data-testid="public-service">
							<div class="min-w-0">
								<p class="truncate font-medium">{service.name}</p>
								<p class="mt-0.5 text-xs text-muted-foreground">
									{#if service.incident_open_since}
										Incident open since {when(service.incident_open_since)}
									{:else if service.success_bps !== null}
										{percent(service.success_bps)} of checks passed in the last 24 hours{#if service.coverage_bps !== null && service.coverage_bps < 10000}
											· {percent(service.coverage_bps)} of scheduled checks ran{/if}
									{:else}
										No checks recorded yet
									{/if}
								</p>
							</div>
							<Badge variant={variantFor(service)} data-state={service.status}>
								{data.state === 'stale' ? `last seen ${service.status}` : service.status}
							</Badge>
						</li>
					{/each}
				</ul>
			{/if}

			{#if data.snap}
				<p class="mt-4 text-xs text-muted-foreground" data-testid="public-updated">
					Updated {ago(data.snap.generated_at, now)} · refreshes automatically
				</p>
			{/if}
		{/if}
	</main>
	<footer class="mx-auto w-full max-w-2xl px-4 pb-8 text-xs text-muted-foreground">
		Status by Service Monitor. “Unknown” means not checked, never assumed healthy.
	</footer>
</div>
