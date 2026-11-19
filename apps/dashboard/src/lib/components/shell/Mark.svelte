<!-- The smon mark: a signal point — the accent's rounded square holding a lit dot. -->
<script lang="ts">
	let { size = 22 }: { size?: number } = $props();
</script>

<span
	class="inline-flex shrink-0 items-center justify-center rounded-md bg-primary shadow-hairline"
	style="width:{size}px;height:{size}px"
>
	<span
		class="rounded-full bg-primary-foreground"
		style="width:{Math.round(size * 0.36)}px;height:{Math.round(size * 0.36)}px"
	></span>
</span>
