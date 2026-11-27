<script lang="ts">
	import ChevronDownIcon from '@lucide/svelte/icons/chevron-down';
	import { cn, type WithElementRef } from "#lib/utils.js";
	import type { HTMLSelectAttributes } from "svelte/elements";

	type NativeSelectProps = Omit<WithElementRef<HTMLSelectAttributes, HTMLSelectElement>, "size"> & {
		size?: "sm" | "default";
	};

	let {
		ref = $bindable(null),
		value = $bindable(),
		class: className,
		size = "default",
		children,
		...restProps
	}: NativeSelectProps = $props();
</script>

<div
	class={cn(
		"group/native-select relative w-fit has-[select:disabled]:opacity-50",
		className
	)}
	data-slot="native-select-wrapper"
	data-size={size}
>
	<select
		bind:value
		bind:this={ref}
		data-slot="native-select"
		data-size={size}
		class="border-input bg-popover placeholder:text-muted-foreground selection:bg-primary selection:text-primary-foreground focus-visible:border-ring focus-visible:ring-4 focus-visible:ring-ring/15 aria-invalid:border-destructive h-11 w-full min-w-0 appearance-none rounded-lg border py-2 pr-9 pl-3.5 text-sm transition-[color,box-shadow,border-color] select-none data-[size=sm]:h-9 outline-none disabled:pointer-events-none disabled:cursor-not-allowed"
		{...restProps}
	>
		{@render children?.()}
	</select>
	<ChevronDownIcon class="text-muted-foreground top-1/2 right-3.5 size-3.5 -translate-y-1/2 pointer-events-none absolute select-none" aria-hidden data-slot="native-select-icon" />
</div>
