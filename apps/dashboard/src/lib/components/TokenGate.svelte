<script lang="ts">
	import * as Card from './ui/card/index.js';
	import { KeyRound } from '@lucide/svelte';
	import { setToken } from '#lib/auth.js';

	// The server answered 401 — it wants a credential, not a retry. Enter it once;
	// it lives in localStorage and rides every apiFetch as `Authorization: Bearer`.
	let value = $state('');
	let error = $state<string | null>(null);

	function submit() {
		const token = value.trim();
		if (!token) {
			error = 'paste the admin token (SMON_ADMIN_TOKEN on the server)';
			return;
		}
		setToken(token);
		location.reload();
	}
</script>

<Card.Root class="border-attention/40 bg-attention-soft">
	<Card.Content class="p-6">
		<div class="flex items-start gap-3">
			<KeyRound class="mt-0.5 size-4 shrink-0 text-attention-ink" />
			<div class="w-full">
				<p class="text-sm font-medium text-attention-ink">The monitor requires a token.</p>
				<p class="mt-0.5 text-sm text-attention-ink/80">
					Paste the server's <code class="font-mono text-xs">SMON_ADMIN_TOKEN</code>. It stays
					in this browser's local storage and is sent as a Bearer credential.
				</p>
				<form
					class="mt-3 flex gap-2"
					onsubmit={(event) => {
						event.preventDefault();
						submit();
					}}
				>
					<input
						bind:value
						type="password"
						placeholder="admin token"
						class="w-full max-w-sm rounded-md border border-input bg-background px-3 py-1.5 font-mono text-sm"
					/>
					<button
						type="submit"
						class="rounded-md bg-primary px-3 py-1.5 text-sm font-medium text-primary-foreground"
					>
						Unlock
					</button>
				</form>
				{#if error}<p class="mt-2 text-sm text-attention-ink">{error}</p>{/if}
			</div>
		</div>
	</Card.Content>
</Card.Root>
