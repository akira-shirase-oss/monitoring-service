<script lang="ts" module>
	import { tv, type VariantProps } from 'tailwind-variants';

	// The status variants speak the monitor's language: `up`, `down` and `unknown` are states,
	// not decorations — `unknown` is deliberately a quiet grey, never a hopeful green.
	export const badgeVariants = tv({
		base: 'inline-flex items-center gap-1.5 rounded-full border px-2.5 py-0.5 text-xs font-medium transition-colors',
		variants: {
			variant: {
				default: 'border-transparent bg-primary text-primary-foreground',
				secondary: 'border-transparent bg-secondary text-secondary-foreground',
				outline: 'border-border text-foreground',
				up: 'border-transparent bg-live-soft text-live-ink',
				down: 'border-transparent bg-attention-soft text-attention-ink',
				unknown: 'border-transparent bg-draft-soft text-draft-ink',
				pending: 'border-transparent bg-pending-soft text-pending-ink'
			}
		},
		defaultVariants: { variant: 'default' }
	});

	export type BadgeVariant = VariantProps<typeof badgeVariants>['variant'];
</script>

<script lang="ts">
	import type { HTMLAttributes } from 'svelte/elements';
	import { cn, type WithElementRef } from '#lib/utils.js';

	type Props = WithElementRef<HTMLAttributes<HTMLSpanElement>> & {
		variant?: BadgeVariant;
	};

	let {
		ref = $bindable(null),
		class: className,
		variant = 'default',
		children,
		...rest
	}: Props = $props();
</script>

<span bind:this={ref} class={cn(badgeVariants({ variant }), className)} {...rest}>
	{@render children?.()}
</span>
