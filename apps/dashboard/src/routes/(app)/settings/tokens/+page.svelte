<script lang="ts">
	import KeyRoundIcon from '@lucide/svelte/icons/key-round';
	import EmptyState from '#lib/components/shell/EmptyState.svelte';
	import PageHeader from '#lib/components/shell/PageHeader.svelte';
</script>

<svelte:head><title>API tokens · Service Monitor</title></svelte:head>

<PageHeader title="API tokens">
	{#snippet description()}Workspace-scoped credentials for scripts — the secret shows once at mint and only its digest is ever stored.{/snippet}
</PageHeader>

<EmptyState icon={KeyRoundIcon} title="Token administration lands in S2e" testid="empty-tokens">
	Minting, the shown-once dialog and revocation ship with the settings slice.
</EmptyState>
