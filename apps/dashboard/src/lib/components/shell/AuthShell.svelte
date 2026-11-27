<!--
  The frame for the pages outside the app (sign-in, sign-up): the sidebar's near-white
  ground, a slim bar with the mark, and one centred column for the auth card. Ported
  from the Studio's Frame — the app pages inside the sidebar get no frame at all.
-->
<script lang="ts">
	import type { Snippet } from 'svelte';
	import { page } from '$app/state';
	import ThemeToggle from './ThemeToggle.svelte';
	import Mark from './Mark.svelte';

	let { children }: { children: Snippet } = $props();
</script>

<div class="flex min-h-svh flex-col bg-sidebar">
	<header class="flex h-14 items-center gap-3 px-4 sm:px-6">
		<a
			href="/"
			class="flex items-center gap-2.5 rounded-md font-medium outline-none focus-visible:ring-[3px] focus-visible:ring-ring/40"
		>
			<Mark />
			<span>Service Monitor</span>
		</a>
		<div class="ml-auto flex items-center gap-1"><ThemeToggle /></div>
	</header>
	<main class="flex flex-1 items-start justify-center px-4 pb-16 pt-10 sm:pt-16">
		{#key page.url.pathname}
			<div class="page-enter w-full max-w-sm">
				{@render children()}
			</div>
		{/key}
	</main>
</div>
