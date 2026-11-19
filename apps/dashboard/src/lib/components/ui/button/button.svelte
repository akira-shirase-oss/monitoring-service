<script lang="ts" module>
	import { type VariantProps, tv } from "tailwind-variants";
	import { cn, type WithElementRef } from "#lib/utils.js";
	import type { HTMLAnchorAttributes, HTMLButtonAttributes } from "svelte/elements";

	// The hierarchy (D-182): one primary per screen, in the accent; everything else is secondary
	// (a neutral sheet on a hairline, which `outline` also names), ghost (no ground until hovered)
	// or destructive (only a confirmation's last step). Flat: no shadows, 8px corners.
	export const buttonVariants = tv({
		base: "group/button inline-flex shrink-0 items-center justify-center gap-2 whitespace-nowrap rounded-control border border-transparent text-sm font-medium transition-[background-color,border-color,color,transform] outline-none select-none focus-visible:border-ring focus-visible:ring-4 focus-visible:ring-ring/25 aria-invalid:border-destructive aria-invalid:ring-4 aria-invalid:ring-destructive/15 active:not-aria-[haspopup]:scale-[0.985] disabled:pointer-events-none disabled:opacity-50 [&_svg]:pointer-events-none [&_svg]:shrink-0 [&_svg:not([class*='size-'])]:size-4",
		variants: {
			variant: {
				default: "bg-primary text-primary-foreground hover:bg-primary-hover aria-expanded:bg-primary-hover",
				secondary: "border-border bg-popover text-foreground hover:bg-accent aria-expanded:bg-accent",
				outline: "border-border bg-popover text-foreground hover:bg-accent aria-expanded:bg-accent",
				ghost: "text-foreground hover:bg-accent aria-expanded:bg-accent",
				destructive: "bg-destructive text-destructive-foreground hover:bg-destructive/90 focus-visible:ring-destructive/25",
				link: "text-foreground underline decoration-border underline-offset-4 hover:decoration-foreground",
			},
			size: {
				default: "h-10 px-4 has-data-[icon=inline-end]:pr-3 has-data-[icon=inline-start]:pl-3",
				xs: "h-7 gap-1 px-2.5 text-xs has-data-[icon=inline-end]:pr-2 has-data-[icon=inline-start]:pl-2 [&_svg:not([class*='size-'])]:size-3.5",
				sm: "h-9 gap-1.5 px-3 has-data-[icon=inline-end]:pr-2.5 has-data-[icon=inline-start]:pl-2.5",
				lg: "h-11 px-5 text-[0.9375rem] has-data-[icon=inline-end]:pr-4 has-data-[icon=inline-start]:pl-4",
				icon: "size-10",
				"icon-xs": "size-7 [&_svg:not([class*='size-'])]:size-3.5",
				"icon-sm": "size-9",
				"icon-lg": "size-11",
			},
		},
		defaultVariants: {
			variant: "default",
			size: "default",
		},
	});

	export type ButtonVariant = VariantProps<typeof buttonVariants>["variant"];
	export type ButtonSize = VariantProps<typeof buttonVariants>["size"];

	export type ButtonProps = WithElementRef<HTMLButtonAttributes> &
		WithElementRef<HTMLAnchorAttributes> & {
			variant?: ButtonVariant;
			size?: ButtonSize;
		};
</script>

<script lang="ts">
	let {
		class: className,
		variant = "default",
		size = "default",
		ref = $bindable(null),
		href = undefined,
		type = "button",
		disabled,
		children,
		...restProps
	}: ButtonProps = $props();
</script>

{#if href}
	<a
		bind:this={ref}
		data-slot="button"
		data-variant={variant}
		class={cn(buttonVariants({ variant, size }), className)}
		href={disabled ? undefined : href}
		aria-disabled={disabled}
		role={disabled ? "link" : undefined}
		tabindex={disabled ? -1 : undefined}
		{...restProps}
	>
		{@render children?.()}
	</a>
{:else}
	<button
		bind:this={ref}
		data-slot="button"
		data-variant={variant}
		class={cn(buttonVariants({ variant, size }), className)}
		{type}
		{disabled}
		{...restProps}
	>
		{@render children?.()}
	</button>
{/if}
