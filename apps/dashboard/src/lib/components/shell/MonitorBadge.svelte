<!--
  Where a monitor stands, as a word on its tone's soft ground — never colour alone (the
  token file's own rule): up is live-green, down is attention-rose, pending is amber,
  unknown and paused are draft-grey. `paused` is a management state, not a verdict, so it
  always wins the word — a paused monitor that last passed is not "up".
-->
<script lang="ts">
	import { Badge, type BadgeVariant } from '#lib/components/ui/badge/index.js';

	let { status, paused = false }: { status: 'up' | 'down' | 'unknown'; paused?: boolean } = $props();

	const variant = $derived<BadgeVariant>(
		paused ? 'draft' : status === 'up' ? 'live' : status === 'down' ? 'attention' : 'draft'
	);
	const word = $derived(paused ? 'paused' : status);
</script>

<Badge {variant} data-testid="monitor-badge" data-state={word}>
	<span
		class="size-1.5 rounded-full {word === 'up'
			? 'bg-live'
			: word === 'down'
				? 'bg-attention'
				: 'bg-draft'}"
		aria-hidden="true"
	></span>{word}
</Badge>
