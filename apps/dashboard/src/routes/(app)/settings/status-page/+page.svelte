<script lang="ts">
	import GlobeIcon from '@lucide/svelte/icons/globe';
	import EmptyState from '#lib/components/shell/EmptyState.svelte';
	import PageHeader from '#lib/components/shell/PageHeader.svelte';
</script>

<svelte:head><title>Status page · Service Monitor</title></svelte:head>

<PageHeader title="Status page">
	{#snippet description()}The public /s/&lt;slug&gt; page — its title, its slug and the monitors the workspace chooses to show.{/snippet}
</PageHeader>

<EmptyState icon={GlobeIcon} title="The status page editor lands in S2f" testid="empty-status-page">
	Editor and public page ship together with the status-page slice.
</EmptyState>
