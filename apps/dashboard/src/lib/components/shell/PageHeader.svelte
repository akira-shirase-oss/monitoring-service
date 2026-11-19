<!-- A page's title (Geist Pixel), a line of context under it, and its actions. The intro
     is one line that says what the page is for, not how the model behind it works. -->
<script lang="ts">
	import type { Snippet } from 'svelte';

	let {
		title,
		eyebrow,
		description,
		actions,
		meta
	}: { title: string; eyebrow?: Snippet; description?: Snippet; actions?: Snippet; meta?: Snippet } =
		$props();
</script>

<header class="mb-8 flex flex-col gap-4 sm:flex-row sm:items-end sm:justify-between">
	<div class="min-w-0 space-y-2">
		{#if eyebrow}<div class="text-sm text-muted-foreground">{@render eyebrow()}</div>{/if}
		<h1 class="page-title">{title}</h1>
		{#if meta}<div class="flex flex-wrap items-center gap-x-3 gap-y-1 text-sm text-muted-foreground">{@render meta()}</div>{/if}
		{#if description}<div class="page-intro max-w-2xl">{@render description()}</div>{/if}
	</div>
	{#if actions}<div class="flex shrink-0 flex-wrap items-center gap-2">{@render actions()}</div>{/if}
</header>
