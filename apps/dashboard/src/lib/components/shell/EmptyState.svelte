<!-- A list with nothing in it yet: a pixel-framed icon, one display-face line saying
     what will be here, and the one action that fills it. Never a paragraph of apology. -->
<script lang="ts">
	import type { Component, Snippet } from 'svelte';

	let {
		icon: Icon,
		title,
		children,
		action,
		testid
	}: { icon: Component; title: string; children?: Snippet; action?: Snippet; testid?: string } =
		$props();
</script>

<div
	class="flex flex-col items-center rounded-card border border-border/70 bg-card px-6 py-12 text-center"
	data-testid={testid}
>
	<div
		class="mb-4 flex size-10 items-center justify-center rounded-lg border border-border bg-popover shadow-hairline"
	>
		<Icon class="size-5 text-muted-foreground" aria-hidden="true" />
	</div>
	<p class="font-display text-base">{title}</p>
	{#if children}<div class="mt-1 max-w-md text-sm text-pretty text-muted-foreground">{@render children()}</div>{/if}
	{#if action}<div class="mt-5 flex flex-wrap justify-center gap-2">{@render action()}</div>{/if}
</div>
