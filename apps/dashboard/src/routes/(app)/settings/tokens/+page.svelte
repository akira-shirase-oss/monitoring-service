<script lang="ts">
	import CheckIcon from '@lucide/svelte/icons/check';
	import CopyIcon from '@lucide/svelte/icons/copy';
	import KeyRoundIcon from '@lucide/svelte/icons/key-round';
	import { invalidateAll } from '$app/navigation';
	import { Badge } from '#lib/components/ui/badge/index.js';
	import { Button } from '#lib/components/ui/button/index.js';
	import * as Card from '#lib/components/ui/card/index.js';
	import * as Dialog from '#lib/components/ui/dialog/index.js';
	import { Input } from '#lib/components/ui/input/index.js';
	import { NativeSelect, NativeSelectOption } from '#lib/components/ui/native-select/index.js';
	import * as Table from '#lib/components/ui/table/index.js';
	import Confirm from '#lib/components/shell/Confirm.svelte';
	import EmptyState from '#lib/components/shell/EmptyState.svelte';
	import PageHeader from '#lib/components/shell/PageHeader.svelte';
	import Problem from '#lib/components/shell/Problem.svelte';
	import { call, shown } from '#lib/call.js';
	import { ago, when } from '#lib/time.js';
	import { current } from '#lib/workspace.svelte.js';
	import type { PageData } from './$types';
	import type { TokenView } from './+page';

	let { data }: { data: PageData } = $props();

	// Minting and revoking are the owner's; members and viewers read the list.
	const owner = $derived(current()?.role === 'owner');

	let creating = $state(false);
	let name = $state('');
	let role = $state<'viewer' | 'member'>('viewer');
	let busy = $state(false);
	let formError = $state<string | null>(null);

	// The secret exists in the mint response and nowhere else: it lives in this
	// variable until the dialog closes, then it is gone for good.
	let secret = $state<string | null>(null);
	let secretName = $state('');
	let copied = $state(false);

	let revoking = $state<TokenView | null>(null);
	let revokeOpen = $state(false);
	let revokeBusy = $state(false);
	let revokeError = $state<string | null>(null);

	async function mint(event: SubmitEvent) {
		event.preventDefault();
		busy = true;
		formError = null;
		const answer = await call<{ token: TokenView & { secret: string } }>('/api/tokens', {
			method: 'POST',
			body: { name: name.trim(), role }
		});
		busy = false;
		if (!answer.ok) {
			formError = answer.failure.message;
			return;
		}
		secret = answer.body.token.secret;
		secretName = answer.body.token.name;
		copied = false;
		creating = false;
		name = '';
		role = 'viewer';
		await invalidateAll();
	}

	async function copy() {
		if (secret === null) return;
		try {
			await navigator.clipboard.writeText(secret);
			copied = true;
		} catch {
			// No clipboard permission: the field is selectable, so the person can copy by hand.
			copied = false;
		}
	}

	async function revoke() {
		if (revoking === null) return;
		revokeBusy = true;
		revokeError = null;
		const answer = await call(`/api/tokens/${revoking.id}`, { method: 'DELETE' });
		revokeBusy = false;
		if (!answer.ok) {
			revokeError = answer.failure.message;
			return;
		}
		revokeOpen = false;
		revoking = null;
		await invalidateAll();
	}
</script>

<svelte:head><title>API tokens · Service Monitor</title></svelte:head>

<PageHeader title="API tokens">
	{#snippet description()}
		Credentials for scripts and integrations. A token works only in this workspace, and its
		secret is shown once, when you create it.
	{/snippet}
	{#snippet actions()}
		{#if owner && !creating && !data.failure}
			<Button size="sm" onclick={() => (creating = true)} data-testid="new-token">New token</Button>
		{/if}
	{/snippet}
</PageHeader>

<div class="flex flex-col gap-6">
	{#if data.failure}
		<Problem error={shown(data.failure)} what="the tokens" retry={invalidateAll} />
	{:else}
		{#if creating}
			<Card.Root>
				<Card.Header>
					<Card.Title>New token</Card.Title>
					<Card.Description>
						A viewer token reads monitors, checks and incidents. A member token can also change
						them. Neither can manage members or other tokens.
					</Card.Description>
				</Card.Header>
				<Card.Content>
					<form class="grid gap-4 sm:grid-cols-[minmax(0,1fr)_12rem_auto] sm:items-end" onsubmit={mint}>
						<div class="space-y-1.5">
							<label for="token-name" class="text-sm font-medium">Name</label>
							<Input
								id="token-name"
								bind:value={name}
								required
								maxlength={80}
								placeholder="deploy script"
								data-testid="token-name"
							/>
						</div>
						<div class="space-y-1.5">
							<label for="token-role" class="text-sm font-medium">Access</label>
							<NativeSelect id="token-role" bind:value={role} class="w-full" data-testid="token-role">
								<NativeSelectOption value="viewer">Viewer (read)</NativeSelectOption>
								<NativeSelectOption value="member">Member (read and write)</NativeSelectOption>
							</NativeSelect>
						</div>
						<div class="flex gap-2">
							<Button
								type="button"
								variant="outline"
								onclick={() => {
									creating = false;
									formError = null;
								}}>Cancel</Button
							>
							<Button type="submit" disabled={busy} data-testid="token-create">
								{busy ? 'Creating…' : 'Create token'}
							</Button>
						</div>
					</form>
					{#if formError}
						<p class="mt-3 text-sm text-attention-ink" role="alert">{formError}</p>
					{/if}
				</Card.Content>
			</Card.Root>
		{/if}

		{#if data.tokens.length === 0 && !creating}
			<EmptyState icon={KeyRoundIcon} title="No API tokens" testid="empty-tokens">
				{#if owner}
					Create one to call the API from a script, a deploy step or another product.
				{:else}
					The workspace owner creates tokens. None exist yet.
				{/if}
				{#snippet action()}
					{#if owner}
						<Button size="sm" onclick={() => (creating = true)}>Create a token</Button>
					{/if}
				{/snippet}
			</EmptyState>
		{:else if data.tokens.length > 0}
			<Card.Root>
				<Card.Content class="p-0">
					<Table.Root data-testid="tokens-table">
						<Table.Header>
							<Table.Row>
								<Table.Head>Name</Table.Head>
								<Table.Head>Access</Table.Head>
								<Table.Head class="hidden sm:table-cell">Created</Table.Head>
								<Table.Head class="hidden sm:table-cell">Last used</Table.Head>
								<Table.Head class="text-right">State</Table.Head>
							</Table.Row>
						</Table.Header>
						<Table.Body>
							{#each data.tokens as token (token.id)}
								<Table.Row data-testid="token-row">
									<Table.Cell class="font-medium">{token.name}</Table.Cell>
									<Table.Cell>{token.role === 'member' ? 'Read and write' : 'Read'}</Table.Cell>
									<Table.Cell class="hidden text-muted-foreground sm:table-cell">{when(token.created_at)}</Table.Cell>
									<Table.Cell class="hidden text-muted-foreground sm:table-cell">{ago(token.last_used_at)}</Table.Cell>
									<Table.Cell class="text-right">
										{#if token.revoked_at}
											<Badge variant="draft" title={when(token.revoked_at)}>Revoked</Badge>
										{:else if owner}
											<Button
												variant="outline"
												size="xs"
												data-testid="token-revoke"
												onclick={() => {
													revoking = token;
													revokeError = null;
													revokeOpen = true;
												}}>Revoke</Button
											>
										{:else}
											<Badge variant="live">Active</Badge>
										{/if}
									</Table.Cell>
								</Table.Row>
							{/each}
						</Table.Body>
					</Table.Root>
				</Card.Content>
			</Card.Root>
		{/if}
	{/if}
</div>

<Dialog.Root
	open={secret !== null}
	onOpenChange={(open) => {
		if (!open) secret = null;
	}}
>
	<Dialog.Content data-testid="token-secret-dialog">
		<Dialog.Header>
			<Dialog.Title>Copy the token for “{secretName}”</Dialog.Title>
			<Dialog.Description>
				This is the only time it is shown. Only a digest is stored, so a lost token can't be
				recovered: revoke it and create another.
			</Dialog.Description>
		</Dialog.Header>
		<div class="flex items-center gap-2">
			<Input
				readonly
				value={secret ?? ''}
				class="font-mono text-xs"
				aria-label="Token secret"
				data-testid="token-secret"
				onfocus={(event: FocusEvent) => (event.currentTarget as HTMLInputElement).select()}
			/>
			<Button variant="outline" onclick={copy} data-testid="token-copy">
				{#if copied}<CheckIcon /> Copied{:else}<CopyIcon /> Copy{/if}
			</Button>
		</div>
		<p class="text-sm text-muted-foreground">
			Send it as <code class="font-mono text-xs">Authorization: Bearer &lt;token&gt;</code>.
		</p>
		<Dialog.Footer>
			<Button onclick={() => (secret = null)} data-testid="token-secret-done">Done</Button>
		</Dialog.Footer>
	</Dialog.Content>
</Dialog.Root>

<Confirm
	bind:open={revokeOpen}
	title="Revoke “{revoking?.name ?? ''}”?"
	confirm="Revoke token"
	busy={revokeBusy}
	onConfirm={revoke}
>
	Anything using this token stops working at once. This can't be undone.
	{#if revokeError}<span class="mt-2 block text-attention-ink" role="alert">{revokeError}</span>{/if}
</Confirm>
