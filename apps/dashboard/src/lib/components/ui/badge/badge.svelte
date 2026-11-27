<script lang="ts" module>
	import { type VariantProps, tv } from "tailwind-variants";

	// Pills. The status tones (live, changed, pending, attention, draft) always pair their colour
	// with a word, and usually a dot or an icon: colour is never the only signal. The Studio's
	// StatusBadge is the one way it shows an entry, request or release state (D-182).
	export const badgeVariants = tv({
		base: "group/badge inline-flex w-fit shrink-0 items-center justify-center gap-1.5 overflow-hidden whitespace-nowrap rounded-full border px-2.5 py-0.5 text-xs font-medium transition-colors focus-visible:ring-4 focus-visible:ring-ring/25 aria-invalid:border-destructive [&>svg]:pointer-events-none [&>svg]:size-3",
		variants: {
			variant: {
				default: "border-transparent bg-primary text-primary-foreground [a]:hover:bg-primary/88",
				secondary: "border-transparent bg-secondary text-secondary-foreground [a]:hover:bg-accent",
				destructive: "border-transparent bg-attention-soft text-attention-ink",
				outline: "border-border bg-popover text-foreground [a]:hover:bg-accent",
				ghost: "border-transparent text-muted-foreground [a]:hover:text-foreground",
				link: "border-transparent text-foreground underline-offset-4 hover:underline",
				live: "border-transparent bg-live-soft text-live-ink",
				changed: "border-transparent bg-changed-soft text-changed-ink",
				pending: "border-transparent bg-pending-soft text-pending-ink",
				attention: "border-transparent bg-attention-soft text-attention-ink",
				draft: "border-transparent bg-draft-soft text-draft-ink",
				brand: "border-transparent bg-brand-soft text-brand-ink",
			},
		},
		defaultVariants: {
			variant: "default",
		},
	});

	export type BadgeVariant = VariantProps<typeof badgeVariants>["variant"];
</script>

<script lang="ts">
	import { cn, type WithElementRef } from "#lib/utils.js";
	import type { HTMLAnchorAttributes } from "svelte/elements";

	let {
		ref = $bindable(null),
		href,
		class: className,
		variant = "default",
		children,
		...restProps
	}: WithElementRef<HTMLAnchorAttributes> & {
		variant?: BadgeVariant;
	} = $props();
</script>

<svelte:element
	this={href ? "a" : "span"}
	bind:this={ref}
	data-slot="badge"
	{href}
	class={cn(badgeVariants({ variant }), className)}
	{...restProps}
>
	{@render children?.()}
</svelte:element>
