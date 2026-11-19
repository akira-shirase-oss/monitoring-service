<script lang="ts">
	import SirenIcon from '@lucide/svelte/icons/siren';
	import EmptyState from '#lib/components/shell/EmptyState.svelte';
	import PageHeader from '#lib/components/shell/PageHeader.svelte';
</script>

<svelte:head><title>Incidents · Service Monitor</title></svelte:head>

<PageHeader title="Incidents">
	{#snippet description()}Open incidents first, then the recently recovered — opened and resolved times are evidence, never estimates.{/snippet}
</PageHeader>

<EmptyState icon={SirenIcon} title="Incidents land in S2d" testid="empty-incidents">
	The list, its durations and its delivery outcomes ship with the incidents slice.
</EmptyState>
