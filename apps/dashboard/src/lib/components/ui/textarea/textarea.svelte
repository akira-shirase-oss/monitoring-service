<script lang="ts">
	import { cn, type WithElementRef, type WithoutChildren } from "#lib/utils.js";
	import type { HTMLTextareaAttributes } from "svelte/elements";

	let {
		ref = $bindable(null),
		value = $bindable(),
		class: className,
		"data-slot": dataSlot = "textarea",
		...restProps
	}: WithoutChildren<WithElementRef<HTMLTextareaAttributes>> = $props();
</script>

<textarea
	bind:this={ref}
	data-slot={dataSlot}
	class={cn(
		"rounded-lg border border-input bg-popover px-3.5 text-base transition-[color,box-shadow,border-color] outline-none md:text-sm placeholder:text-muted-foreground/80 focus-visible:border-ring focus-visible:ring-4 focus-visible:ring-ring/15 aria-invalid:border-destructive aria-invalid:ring-4 aria-invalid:ring-destructive/10 disabled:cursor-not-allowed disabled:opacity-50 flex field-sizing-content min-h-24 w-full resize-none py-3",
		className
	)}
	bind:value
	{...restProps}></textarea>
