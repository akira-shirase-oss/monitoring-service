<script lang="ts">
	import { Dialog as DialogPrimitive } from "bits-ui";
	import { cn } from "#lib/utils.js";

	let {
		ref = $bindable(null),
		class: className,
		...restProps
	}: DialogPrimitive.TitleProps = $props();
</script>

<DialogPrimitive.Title
	bind:ref
	data-slot="dialog-title"
	class={cn("font-display text-xl leading-tight font-[560] tracking-[-0.01em] pr-10", className)}
	{...restProps}
/>
