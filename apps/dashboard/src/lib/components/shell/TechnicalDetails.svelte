<!--
  The internal names behind a screen, for operators and for support: ids, hashes,
  API addresses, the words the API and the CLI use. Closed until someone asks for it.
-->
<script lang="ts">
	import ChevronRightIcon from '@lucide/svelte/icons/chevron-right';
	import type { Snippet } from 'svelte';

	let { children, class: className = '' }: { children: Snippet; class?: string } = $props();
</script>

<details class="group text-sm {className}" data-technical data-testid="technical-details">
	<summary
		class="inline-flex cursor-pointer list-none items-center gap-1 rounded-md text-xs text-muted-foreground outline-none select-none hover:text-foreground focus-visible:ring-[3px] focus-visible:ring-ring/40 [&::-webkit-details-marker]:hidden"
	>
		<ChevronRightIcon class="size-3.5 transition-transform group-open:rotate-90" aria-hidden="true" />Technical
		details
	</summary>
	<div class="mt-2 space-y-1.5 rounded-lg border border-dashed border-border bg-muted/40 px-3 py-2.5 text-xs">
		{@render children()}
	</div>
</details>
