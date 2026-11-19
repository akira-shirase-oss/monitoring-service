<script lang="ts">
	import WebhookIcon from '@lucide/svelte/icons/webhook';
	import EmptyState from '#lib/components/shell/EmptyState.svelte';
	import PageHeader from '#lib/components/shell/PageHeader.svelte';
</script>

<svelte:head><title>Alerts · Service Monitor</title></svelte:head>

<PageHeader title="Alerts">
	{#snippet description()}The workspace's signed webhook — validated at save and inside every send — plus its test alert.{/snippet}
</PageHeader>

<EmptyState icon={WebhookIcon} title="The alert channel lands in S2e" testid="empty-alerts">
	The webhook card — URL, secret, send test alert — ships with the settings slice.
</EmptyState>
