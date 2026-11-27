<!--
  The account behind the session: change its password, or delete it. Both ask for the
  password again, and the server's answer is shown as it came — a refusal to delete
  names the workspace that still needs an owner. The operator's session has no account,
  so it gets an explanation instead of forms that could only fail.
-->
<script lang="ts">
	import { Button } from '#lib/components/ui/button/index.js';
	import * as Card from '#lib/components/ui/card/index.js';
	import { Input } from '#lib/components/ui/input/index.js';
	import Confirm from '#lib/components/shell/Confirm.svelte';
	import FormRow from '#lib/components/shell/FormRow.svelte';
	import PageHeader from '#lib/components/shell/PageHeader.svelte';
	import { call } from '#lib/call.js';
	import type { ApiFailure } from '#lib/api.js';
	import { pick } from '#lib/workspace.svelte.js';
	import type { PageData } from './$types';

	let { data }: { data: PageData } = $props();

	// Only an account has a password and can be deleted. The operator signs in with the
	// server's admin token; its session carries no account.
	const account = $derived(data.account);

	let currentPassword = $state('');
	let newPassword = $state('');
	let changing = $state(false);
	let changeError = $state<string | null>(null);
	let changed = $state(false);

	let confirming = $state(false);
	let deletePassword = $state('');
	let deleting = $state(false);
	let deleteError = $state<string | null>(null);

	/** The server's words, except its terse 401, which here can only mean one thing. */
	function words(failure: ApiFailure): string {
		return failure.status === 401 && failure.message === 'invalid credentials'
			? "That password isn't right."
			: failure.message;
	}

	async function changePassword(event: SubmitEvent) {
		event.preventDefault();
		changing = true;
		changeError = null;
		changed = false;
		const answer = await call('/api/account/password', {
			method: 'POST',
			body: { current_password: currentPassword, new_password: newPassword }
		});
		changing = false;
		if (!answer.ok) {
			changeError = words(answer.failure);
			return;
		}
		currentPassword = '';
		newPassword = '';
		changed = true;
	}

	async function deleteAccount() {
		deleting = true;
		deleteError = null;
		const answer = await call('/api/account', {
			method: 'DELETE',
			body: { password: deletePassword }
		});
		if (!answer.ok) {
			deleting = false;
			deleteError = words(answer.failure);
			return;
		}
		// The account and its session are gone: forget the picked workspace and load the
		// sign-in page fresh, so nothing of the old session stays in memory.
		pick('');
		window.location.assign('/login');
	}
</script>

<svelte:head><title>Account · Service Monitor</title></svelte:head>

<PageHeader title="Account">
	{#snippet description()}
		Your password and your account. These belong to you, not to one workspace.
	{/snippet}
</PageHeader>

{#if account === null}
	<Card.Root data-testid="account-operator">
		<Card.Header>
			<Card.Title>This session has no account</Card.Title>
			<Card.Description>
				You're signed in as the operator, with the server's admin token. There is no password
				to change here and no account to delete. To end this session, sign out; to stop the
				token working, change it in the server's configuration.
			</Card.Description>
		</Card.Header>
	</Card.Root>
{:else}
	<div class="flex flex-col gap-6">
		<Card.Root>
			<Card.Header>
				<Card.Title>Password</Card.Title>
				<Card.Description>
					Signed in as <span class="font-medium text-foreground" data-user-content>{account.email}</span>.
					Changing the password signs out every other device.
				</Card.Description>
			</Card.Header>
			<Card.Content>
				<form onsubmit={changePassword}>
					<FormRow label="Current password" for="current-password">
						<Input
							id="current-password"
							type="password"
							autocomplete="current-password"
							bind:value={currentPassword}
							required
							data-testid="current-password"
							oninput={() => (changed = false)}
						/>
					</FormRow>
					<FormRow label="New password" hint="8 to 256 characters." for="new-password">
						<Input
							id="new-password"
							type="password"
							autocomplete="new-password"
							bind:value={newPassword}
							required
							minlength={8}
							maxlength={256}
							data-testid="new-password"
							oninput={() => (changed = false)}
						/>
					</FormRow>
					<div class="flex flex-wrap items-center gap-3 pt-5">
						<Button type="submit" disabled={changing} data-testid="change-password">
							{changing ? 'Changing…' : 'Change password'}
						</Button>
						{#if changeError}
							<p class="text-sm text-attention-ink" role="alert" data-testid="password-error">
								{changeError}
							</p>
						{:else if changed}
							<p class="text-sm text-muted-foreground" role="status" data-testid="password-changed">
								Password changed. Your other devices were signed out; this one stays signed in.
							</p>
						{/if}
					</div>
				</form>
			</Card.Content>
		</Card.Root>

		<Card.Root>
			<Card.Header>
				<Card.Title>Delete account</Card.Title>
				<Card.Description>
					Deletes your account and every workspace only you are in, with its monitors, checks,
					incidents, alerts and status page. Workspaces you share with other people stay; you
					leave them. This can't be undone. For a copy first, an owner can export a workspace's
					data from <a class="underline" href="/settings/general">General</a>.
				</Card.Description>
			</Card.Header>
			<Card.Content>
				<Button
					variant="outline"
					data-testid="delete-account"
					onclick={() => {
						deletePassword = '';
						deleteError = null;
						confirming = true;
					}}>Delete account…</Button
				>
			</Card.Content>
		</Card.Root>
	</div>

	<Confirm
		bind:open={confirming}
		title="Delete your account?"
		confirm="Delete account"
		busy={deleting}
		disabled={deletePassword === ''}
		onConfirm={deleteAccount}
	>
		{account.email} is deleted, together with every workspace only you are in: its monitors,
		checks, incidents, alerts and status page. Shared workspaces stay, without you. This can't
		be undone.
		{#snippet extra()}
			<div class="space-y-1.5">
				<label for="delete-password" class="text-sm font-medium">Your password</label>
				<Input
					id="delete-password"
					type="password"
					autocomplete="current-password"
					bind:value={deletePassword}
					data-testid="delete-password"
				/>
				{#if deleteError}
					<p class="text-sm text-attention-ink" role="alert" data-testid="delete-error">
						{deleteError}
					</p>
				{/if}
			</div>
		{/snippet}
	</Confirm>
{/if}
