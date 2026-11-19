<!--
  One button between light and dark. Until it is pressed the dashboard follows the system's
  scheme; pressing it onto the system's own scheme goes back to following the system
  (theme.ts). The name stays "Dark theme" and the pressed state says which is on, so a
  screen reader hears a toggle, not a label that changes under it.
-->
<script lang="ts">
	import MoonIcon from '@lucide/svelte/icons/moon';
	import SunIcon from '@lucide/svelte/icons/sun';
	import { mode, userPrefersMode } from 'mode-watcher';
	import { Button } from '#lib/components/ui/button/index.js';
	import { toggleTheme } from './theme.js';

	let button = $state<HTMLElement | null>(null);
	const dark = $derived(mode.current === 'dark');
	const hint = $derived(
		`${dark ? 'Switch to light' : 'Switch to dark'}${userPrefersMode.current === 'system' ? ' (following your system)' : ''}`
	);
</script>

<Button
	bind:ref={button}
	variant="ghost"
	size="icon-sm"
	class="relative"
	aria-label="Dark theme"
	aria-pressed={dark}
	title={hint}
	onclick={() => toggleTheme(button)}
>
	<SunIcon class="theme-icon rotate-0 scale-100 dark:-rotate-90 dark:scale-0" aria-hidden="true" />
	<MoonIcon class="theme-icon absolute rotate-90 scale-0 dark:rotate-0 dark:scale-100" aria-hidden="true" />
</Button>
