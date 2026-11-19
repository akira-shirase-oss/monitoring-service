<!-- One row of a form: what it is and why, then the field, a rule under. Stacked unless
     the form is wide, when the label sits to the left — the row's own width decides. -->
<script lang="ts">
	import type { Snippet } from 'svelte';

	let {
		label,
		hint,
		for: htmlFor,
		children
	}: { label: string; hint?: string; for?: string; children: Snippet } = $props();
</script>

<div class="@container rule-dotted py-5 first:pt-0 last:border-b-0">
	<div class="grid gap-x-10 gap-y-2 @3xl:grid-cols-[minmax(0,15rem)_minmax(0,1fr)]">
		<div class="space-y-1">
			{#if htmlFor}
				<label for={htmlFor} class="text-sm font-medium">{label}</label>
			{:else}
				<p class="text-sm font-medium">{label}</p>
			{/if}
			{#if hint}<p class="text-sm text-muted-foreground">{hint}</p>{/if}
		</div>
		<div class="min-w-0">{@render children()}</div>
	</div>
</div>
