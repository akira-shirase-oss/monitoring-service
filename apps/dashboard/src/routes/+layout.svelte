<script lang="ts">
	import '../app.css';
	import { ModeWatcher } from 'mode-watcher';
	import { Toaster } from '#lib/components/ui/sonner/index.js';
	import * as Tooltip from '#lib/components/ui/tooltip/index.js';
	import type { LayoutProps } from './$types';

	let { children }: LayoutProps = $props();
</script>

<svelte:head>
	<title>Service Monitor</title>
</svelte:head>

<ModeWatcher defaultMode="system" />
<Tooltip.Provider delayDuration={300}>
	{@render children()}
</Tooltip.Provider>
<Toaster position="bottom-right" />
