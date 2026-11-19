<script lang="ts">
	import UsersIcon from '@lucide/svelte/icons/users';
	import EmptyState from '#lib/components/shell/EmptyState.svelte';
	import PageHeader from '#lib/components/shell/PageHeader.svelte';
</script>

<svelte:head><title>Members · Service Monitor</title></svelte:head>

<PageHeader title="Members">
	{#snippet description()}Who can see and change this workspace — owners administer, members work, viewers read.{/snippet}
</PageHeader>

<EmptyState icon={UsersIcon} title="The roster lands in S2e" testid="empty-members">
	Member administration — invite by email, change roles, remove — ships with the settings slice.
</EmptyState>
