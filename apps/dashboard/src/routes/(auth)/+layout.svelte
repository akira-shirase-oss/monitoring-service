<script lang="ts">
	import AuthShell from '#lib/components/shell/AuthShell.svelte';
	import type { LayoutProps } from './$types';

	let { children }: LayoutProps = $props();
</script>

<AuthShell>{@render children()}</AuthShell>
