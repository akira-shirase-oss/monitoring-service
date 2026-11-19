<script lang="ts">
	import CalendarClockIcon from '@lucide/svelte/icons/calendar-clock';
	import EmptyState from '#lib/components/shell/EmptyState.svelte';
	import PageHeader from '#lib/components/shell/PageHeader.svelte';
</script>

<svelte:head><title>Maintenance · Service Monitor</title></svelte:head>

<PageHeader title="Maintenance">
	{#snippet description()}Windows that suppress the page and never the evidence — a slot missed for maintenance still counts as missed.{/snippet}
</PageHeader>

<EmptyState icon={CalendarClockIcon} title="Maintenance windows land in S2d" testid="empty-maintenance">
	The workspace-wide window list ships with the incidents slice.
</EmptyState>
