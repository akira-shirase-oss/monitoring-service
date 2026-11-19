<script lang="ts">
	import RocketIcon from '@lucide/svelte/icons/rocket';
	import EmptyState from '#lib/components/shell/EmptyState.svelte';
	import PageHeader from '#lib/components/shell/PageHeader.svelte';
</script>

<svelte:head><title>Getting started · Service Monitor</title></svelte:head>

<PageHeader title="Getting started">
	{#snippet description()}Name the workspace, watch the first URL, connect an alert channel — then the first checks land.{/snippet}
</PageHeader>

<EmptyState icon={RocketIcon} title="Onboarding lands in S2c" testid="empty-onboarding">
	The first-run checklist — every item derived from real state — ships with the onboarding slice.
</EmptyState>
