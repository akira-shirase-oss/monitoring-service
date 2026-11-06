<script lang="ts">
	import '../app.css';
	import { logout } from '#lib/auth.js';

	let { children } = $props();

	// Sign-out is idempotent: no session → the POST is a no-op and the reload
	// just lands back here; a session → the row dies server-side, not just in the tab.
	async function signOut() {
		await logout();
		location.reload();
	}
</script>

<div class="flex min-h-dvh flex-col">
	<header class="border-b">
		<div class="mx-auto flex h-14 w-full max-w-5xl items-center justify-between px-6">
			<a href="/" class="flex items-center gap-2.5 font-semibold tracking-tight">
				<span class="size-2.5 rounded-full bg-brand"></span>
				Service Monitor
			</a>
			<nav class="flex items-center gap-4 text-sm text-muted-foreground">
				<a href="/" class="hover:text-foreground">Monitors</a>
				<button onclick={signOut} class="hover:text-foreground">Sign out</button>
			</nav>
		</div>
	</header>
	<main class="mx-auto w-full max-w-5xl flex-1 px-6 py-8">
		{@render children()}
	</main>
</div>
