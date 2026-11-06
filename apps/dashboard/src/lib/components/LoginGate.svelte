<script lang="ts">
	import * as Card from './ui/card/index.js';
	import { KeyRound } from '@lucide/svelte';
	import { login } from '#lib/auth.js';
	import { readEnvelope } from '#lib/api.js';

	// The server answered 401 — it wants a credential, not a retry. The token is
	// presented once to /api/login and exchanged for an HttpOnly session cookie;
	// it is never stored in this browser.
	let value = $state('');
	let error = $state<string | null>(null);
	let busy = $state(false);

	async function submit() {
		const token = value.trim();
		if (!token) {
			error = 'enter the admin token (SMON_ADMIN_TOKEN on the server)';
			return;
		}
		busy = true;
		error = null;
		try {
			const response = await login(token);
			const failure = await readEnvelope(response);
			if (failure === null) {
				location.reload();
				return;
			}
			error = failure.status === 401 ? 'That token was refused.' : failure.message;
		} catch {
			error = 'the monitor server is unreachable — check it, then try again';
		} finally {
			busy = false;
		}
	}
</script>

<Card.Root class="border-attention/40 bg-attention-soft">
	<Card.Content class="p-6">
		<div class="flex items-start gap-3">
			<KeyRound class="mt-0.5 size-4 shrink-0 text-attention-ink" />
			<div class="w-full">
				<p class="text-sm font-medium text-attention-ink">Sign in to manage monitors.</p>
				<p class="mt-0.5 text-sm text-attention-ink/80">
					Enter the server's <code class="font-mono text-xs">SMON_ADMIN_TOKEN</code>. It is
					exchanged once for a session cookie — never stored in this browser.
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
						autocomplete="current-password"
						class="w-full max-w-sm rounded-md border border-input bg-background px-3 py-1.5 font-mono text-sm"
					/>
					<button
						type="submit"
						disabled={busy}
						class="rounded-md bg-primary px-3 py-1.5 text-sm font-medium text-primary-foreground disabled:opacity-50"
					>
						{busy ? 'Signing in…' : 'Sign in'}
					</button>
				</form>
				{#if error}<p class="mt-2 text-sm text-attention-ink">{error}</p>{/if}
			</div>
		</div>
	</Card.Content>
</Card.Root>
