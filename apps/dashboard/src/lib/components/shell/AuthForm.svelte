<!--
  Signing in with an email and a password, or creating that account (invite-gated, S1b).
  The session cookie comes back in the response itself; a successful answer reloads the
  app onto `returnTo`. The server's message is shown verbatim — a uniform 403 for a bad
  invite, 401 for a bad pair, and Retry-After's seconds with a 429 or busy 503. The
  operator token path folds away under technical details — it's for the CLI's owner.
-->
<script lang="ts">
	import { goto, invalidateAll } from '$app/navigation';
	import { page } from '$app/state';
	import { untrack } from 'svelte';
	import { Button } from '#lib/components/ui/button/index.js';
	import { Input } from '#lib/components/ui/input/index.js';
	import { Label } from '#lib/components/ui/label/index.js';
	import * as Card from '#lib/components/ui/card/index.js';
	import { login, loginToken, signup } from '#lib/auth.js';
	import { readEnvelope } from '#lib/api.js';
	import TechnicalDetails from './TechnicalDetails.svelte';

	let { start = 'signin' }: { start?: 'signin' | 'signup' } = $props();

	let mode = $state<'signin' | 'signup'>(untrack(() => start));
	let email = $state('');
	let password = $state('');
	let invite = $state('');
	let token = $state('');
	let error = $state('');
	let busy = $state(false);

	// Only same-site paths, ever (the server's own rule for post-login jumps).
	const returnTo = $derived.by(() => {
		const back = page.url.searchParams.get('return') ?? '/';
		return back.startsWith('/') && !back.startsWith('//') ? back : '/';
	});

	async function words(response: Response): Promise<string> {
		const failure = await readEnvelope(response);
		if (failure === null) return '';
		const wait = response.headers.get('retry-after');
		if ((failure.status === 429 || failure.status === 503) && wait) {
			return `${failure.message} (retry in ${wait}s)`;
		}
		return failure.message;
	}

	async function submit(event: SubmitEvent) {
		event.preventDefault();
		busy = true;
		error = '';
		try {
			let response: Response;
			if (mode === 'signup') {
				response = await signup(email.trim(), password, invite.trim());
				if (response.ok) response = await login(email.trim(), password);
			} else {
				response = await login(email.trim(), password);
			}
			if (!response.ok) {
				error = await words(response);
				return;
			}
			await invalidateAll();
			await goto(returnTo);
		} catch {
			error = 'The monitor server is unreachable — check it, then try again.';
		} finally {
			busy = false;
		}
	}

	async function submitToken() {
		busy = true;
		error = '';
		try {
			const response = await loginToken(token.trim());
			if (!response.ok) {
				error = await words(response);
				return;
			}
			await invalidateAll();
			await goto(returnTo);
		} catch {
			error = 'The monitor server is unreachable — check it, then try again.';
		} finally {
			busy = false;
		}
	}
</script>

<Card.Root data-testid="auth-card">
	<Card.Header>
		<Card.Title class="page-title !text-xl">{mode === 'signup' ? 'Create your account' : 'Sign in'}</Card.Title>
		<Card.Description>
			{mode === 'signup'
				? 'An invite code opens a workspace on this monitor.'
				: 'Your account session watches the workspaces you belong to.'}
		</Card.Description>
	</Card.Header>
	<Card.Content>
		<form onsubmit={submit} data-testid="password-auth" class="space-y-3">
			<div class="space-y-1">
				<Label for="auth-email">Email</Label>
				<Input id="auth-email" type="email" bind:value={email} required maxlength={254} autocomplete="email" />
			</div>
			<div class="space-y-1">
				<Label for="auth-password">Password</Label>
				<Input
					id="auth-password"
					type="password"
					bind:value={password}
					required
					minlength={8}
					maxlength={256}
					autocomplete={mode === 'signup' ? 'new-password' : 'current-password'}
				/>
				{#if mode === 'signup'}<p class="text-sm text-muted-foreground">At least 8 characters.</p>{/if}
			</div>
			{#if mode === 'signup'}
				<div class="space-y-1">
					<Label for="auth-invite">Invite code</Label>
					<Input id="auth-invite" bind:value={invite} required maxlength={128} autocomplete="off" class="font-mono" />
					<p class="text-sm text-muted-foreground">Shown once by whoever runs the monitor.</p>
				</div>
			{/if}
			{#if error}<p role="alert" class="pt-2 text-sm text-destructive" data-testid="auth-error">{error}</p>{/if}
			<div class="pt-4">
				<Button type="submit" class="w-full" disabled={busy} data-testid="password-submit">
					{busy ? 'Working…' : mode === 'signup' ? 'Create account' : 'Sign in'}
				</Button>
			</div>
		</form>
		<p class="pt-4 text-center text-sm text-muted-foreground">
			{#if mode === 'signin'}
				Have an invite? <a class="underline underline-offset-2 hover:text-foreground" href="/signup{page.url.search}">Create an account</a>
			{:else}
				Already have an account? <a class="underline underline-offset-2 hover:text-foreground" href="/login{page.url.search}">Sign in</a>
			{/if}
		</p>
		{#if mode === 'signin'}
			<TechnicalDetails class="mt-2">
				<p class="pb-1 text-muted-foreground">The operator's admin token — for whoever runs the monitor itself.</p>
				<form
					class="flex gap-2"
					onsubmit={(event) => {
						event.preventDefault();
						submitToken();
					}}
				>
					<Input
						type="password"
						bind:value={token}
						placeholder="SMON_ADMIN_TOKEN"
						autocomplete="off"
						class="font-mono"
						data-testid="token-input"
					/>
					<Button type="submit" variant="outline" disabled={busy || token.trim() === ''} data-testid="token-submit">Sign in</Button>
				</form>
			</TechnicalDetails>
		{/if}
	</Card.Content>
</Card.Root>
