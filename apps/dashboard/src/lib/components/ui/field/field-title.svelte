<script lang="ts">
	import { cn, type WithElementRef } from "#lib/utils.js";
	import type { HTMLAttributes } from "svelte/elements";

	let {
		ref = $bindable(null),
		class: className,
		children,
		...restProps
	}: WithElementRef<HTMLAttributes<HTMLDivElement>> = $props();
</script>

<div
	bind:this={ref}
	data-slot="field-title"
	class={cn("gap-2 group-data-[disabled=true]/field:opacity-50 leading-normal font-semibold in-data-[slot=field-label]:font-semibold text-xs flex w-fit items-center", className)}
	{...restProps}
>
	{@render children?.()}
</div>
