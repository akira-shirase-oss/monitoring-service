<script lang="ts">
	import { cn, type WithElementRef } from "#lib/utils.js";
	import type { HTMLAttributes } from "svelte/elements";

	let {
		ref = $bindable(null),
		class: className,
		children,
		size = "default",
		...restProps
	}: WithElementRef<HTMLAttributes<HTMLDivElement>> & { size?: "default" | "sm" } = $props();
</script>

<div
	bind:this={ref}
	data-slot="card"
	data-size={size}
	class={cn("group/card flex flex-col gap-(--card-spacing) overflow-hidden rounded-card border border-border/70 bg-card py-(--card-spacing) text-sm text-card-foreground [--card-spacing:var(--card-padding)] has-[>img:first-child]:pt-0 data-[size=sm]:[--card-spacing:var(--card-padding-sm)] *:[img:first-child]:rounded-t-card", className)}
	{...restProps}
>
	{@render children?.()}
</div>
