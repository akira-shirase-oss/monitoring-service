<!--
  Who is signed in, and signing out, from the sidebar's foot. Signing out ends the session
  on the server first — the row dies server-side, not just in this tab — then the page
  reloads onto the sign-in screen, so nothing of the old session stays in memory. If the
  server can't be reached it says so and stays signed in: it never claims a sign-out
  that didn't happen.
-->
<script lang="ts">
	import LogOutIcon from '@lucide/svelte/icons/log-out';
	import { toast } from 'svelte-sonner';
	import { buttonVariants } from '#lib/components/ui/button/index.js';
	import * as DropdownMenu from '#lib/components/ui/dropdown-menu/index.js';
	import { logout } from '#lib/auth.js';

	let { email, role }: { email: string; role: string } = $props();

	const initials = $derived(
		(email.split('@')[0] ?? '')
			.split(/[.\-_+]/)
			.filter((word) => word !== '')
			.slice(0, 2)
			.map((word) => word.slice(0, 1).toUpperCase())
			.join('') || '?'
	);
	let busy = $state(false);

	async function leave() {
		busy = true;
		try {
			await logout();
			window.location.assign('/login');
		} catch {
			busy = false;
			toast.error("You're still signed in. The server didn't answer — try again.");
		}
	}
</script>

<DropdownMenu.Root>
	<DropdownMenu.Trigger
		class="{buttonVariants({ variant: 'ghost', size: 'icon-sm' })} rounded-full"
		data-testid="account-menu"
	>
		<span
			class="flex size-8 items-center justify-center rounded-full border border-border bg-secondary text-sm font-semibold"
			aria-hidden="true">{initials}</span
		><span class="sr-only">Account: {email}</span>
	</DropdownMenu.Trigger>
	<DropdownMenu.Content align="end" class="w-60">
		<DropdownMenu.Label class="grid gap-0.5 font-normal">
			<span class="text-xs text-muted-foreground">Signed in as</span>
			<span class="truncate text-sm font-medium" data-user-content>{email}</span>
			<span class="text-xs text-muted-foreground">role: {role}</span>
		</DropdownMenu.Label>
		<DropdownMenu.Separator />
		<DropdownMenu.Item onSelect={leave} disabled={busy} data-testid="sign-out">
			<LogOutIcon />{busy ? 'Signing out…' : 'Sign out'}
		</DropdownMenu.Item>
	</DropdownMenu.Content>
</DropdownMenu.Root>
