<script lang="ts">
	import { invalidateAll } from '$app/navigation';
	import { toast } from 'svelte-sonner';
	import { Badge } from '#lib/components/ui/badge/index.js';
	import { Button } from '#lib/components/ui/button/index.js';
	import * as Card from '#lib/components/ui/card/index.js';
	import { Input } from '#lib/components/ui/input/index.js';
	import { NativeSelect, NativeSelectOption } from '#lib/components/ui/native-select/index.js';
	import * as Table from '#lib/components/ui/table/index.js';
	import Confirm from '#lib/components/shell/Confirm.svelte';
	import PageHeader from '#lib/components/shell/PageHeader.svelte';
	import Problem from '#lib/components/shell/Problem.svelte';
	import { call, shown } from '#lib/call.js';
	import { when } from '#lib/time.js';
	import { current } from '#lib/workspace.svelte.js';
	import type { PageData } from './$types';
	import type { MemberView, Role } from './+page';

	let { data }: { data: PageData } = $props();

	const owner = $derived(current()?.role === 'owner');
	const ROLE_WORDS: Record<Role, string> = {
		owner: 'Owner',
		member: 'Member',
		viewer: 'Viewer'
	};

	let email = $state('');
	let role = $state<Role>('viewer');
	let inviteBusy = $state(false);
	let inviteError = $state<string | null>(null);
	let inviteNote = $state<string | null>(null);

	let rowError = $state<string | null>(null);
	let rowBusy = $state<string | null>(null);

	let removing = $state<MemberView | null>(null);
	let removeOpen = $state(false);
	let removeBusy = $state(false);
	let removeError = $state<string | null>(null);

	async function invite(event: SubmitEvent) {
		event.preventDefault();
		inviteBusy = true;
		inviteError = null;
		inviteNote = null;
		const sentTo = email.trim();
		const answer = await call('/api/members', { method: 'POST', body: { email: sentTo, role } });
		inviteBusy = false;
		if (!answer.ok) {
			inviteError = answer.failure.message;
			return;
		}
		// The server answers the same way whether or not the address has an account, so
		// this can't promise a seat — only say what happens next.
		inviteNote = `Invitation recorded for ${sentTo}. It appears under “Invitations for you” on their Members page once they have an account with that address.`;
		email = '';
		role = 'viewer';
		await invalidateAll();
	}

	async function setRole(member: MemberView, next: Role) {
		if (next === member.role) return;
		rowBusy = member.account;
		rowError = null;
		const answer = await call(`/api/members/${member.account}`, {
			method: 'PATCH',
			body: { role: next }
		});
		rowBusy = null;
		if (!answer.ok) rowError = answer.failure.message;
		// Reload either way: a refused change must put the select back on the stored role.
		await invalidateAll();
	}

	async function remove() {
		if (removing === null) return;
		removeBusy = true;
		removeError = null;
		const answer = await call(`/api/members/${removing.account}`, { method: 'DELETE' });
		removeBusy = false;
		if (!answer.ok) {
			removeError = answer.failure.message;
			return;
		}
		removeOpen = false;
		removing = null;
		await invalidateAll();
	}

	async function withdraw(id: string) {
		rowBusy = id;
		rowError = null;
		const answer = await call(`/api/members/invitations/${id}`, { method: 'DELETE' });
		rowBusy = null;
		if (!answer.ok) rowError = answer.failure.message;
		await invalidateAll();
	}

	async function respond(id: string, verb: 'accept' | 'decline', workspace: string) {
		rowBusy = id;
		rowError = null;
		const answer = await call(`/api/invitations/${id}/${verb}`, { method: 'POST', body: {} });
		rowBusy = null;
		if (!answer.ok) {
			rowError = answer.failure.message;
		} else if (verb === 'accept') {
			toast.success(`You joined ${workspace}. Switch to it from the workspace menu.`);
		}
		await invalidateAll();
	}
</script>

<svelte:head><title>Members · Service Monitor</title></svelte:head>

<PageHeader title="Members">
	{#snippet description()}
		Who can see and change this workspace. Owners manage people and tokens, members manage
		monitors, viewers only read.
	{/snippet}
</PageHeader>

<div class="flex flex-col gap-6">
	{#if rowError}
		<p class="text-sm text-attention-ink" role="alert" data-testid="members-error">{rowError}</p>
	{/if}

	{#if data.received.length > 0}
		<Card.Root data-testid="received-invitations">
			<Card.Header>
				<Card.Title>Invitations for you</Card.Title>
				<Card.Description>Other workspaces that invited this account.</Card.Description>
			</Card.Header>
			<Card.Content class="flex flex-col gap-3">
				{#each data.received as invitation (invitation.id)}
					<div class="flex flex-wrap items-center justify-between gap-3">
						<p class="text-sm">
							<span class="font-medium">{invitation.workspace}</span>
							<span class="text-muted-foreground">
								· as {ROLE_WORDS[invitation.role].toLowerCase()} · sent {when(invitation.created_at)}</span
							>
						</p>
						<div class="flex gap-2">
							<Button
								variant="outline"
								size="sm"
								disabled={rowBusy === invitation.id}
								onclick={() => respond(invitation.id, 'decline', invitation.workspace)}>Decline</Button
							>
							<Button
								size="sm"
								disabled={rowBusy === invitation.id}
								data-testid="invitation-accept"
								onclick={() => respond(invitation.id, 'accept', invitation.workspace)}>Join</Button
							>
						</div>
					</div>
				{/each}
			</Card.Content>
		</Card.Root>
	{/if}

	{#if data.failure}
		<Problem error={shown(data.failure)} what="the members" retry={invalidateAll} />
	{:else}
		{#if owner}
			<Card.Root>
				<Card.Header>
					<Card.Title>Invite someone</Card.Title>
					<Card.Description>
						They need a Service Monitor account with this email address. The seat is theirs once
						they accept. To make someone an owner, change their role after they join.
					</Card.Description>
				</Card.Header>
				<Card.Content>
					<form class="grid gap-4 sm:grid-cols-[minmax(0,1fr)_11rem_auto] sm:items-end" onsubmit={invite}>
						<div class="space-y-1.5">
							<label for="invite-email" class="text-sm font-medium">Email</label>
							<Input
								id="invite-email"
								type="email"
								bind:value={email}
								required
								autocomplete="off"
								placeholder="name@example.com"
								data-testid="invite-email"
							/>
						</div>
						<div class="space-y-1.5">
							<label for="invite-role" class="text-sm font-medium">Role</label>
							<NativeSelect id="invite-role" bind:value={role} class="w-full" data-testid="invite-role">
								<NativeSelectOption value="viewer">Viewer</NativeSelectOption>
								<NativeSelectOption value="member">Member</NativeSelectOption>
							</NativeSelect>
						</div>
						<Button type="submit" disabled={inviteBusy} data-testid="invite-send">
							{inviteBusy ? 'Inviting…' : 'Invite'}
						</Button>
					</form>
					{#if inviteError}
						<p class="mt-3 text-sm text-attention-ink" role="alert">{inviteError}</p>
					{/if}
					{#if inviteNote}
						<p class="mt-3 text-sm text-muted-foreground" role="status" data-testid="invite-note">
							{inviteNote}
						</p>
					{/if}
				</Card.Content>
			</Card.Root>
		{/if}

		<Card.Root>
			<Card.Content class="p-0">
				<Table.Root data-testid="members-table">
					<Table.Header>
						<Table.Row>
							<Table.Head>Email</Table.Head>
							<Table.Head>Role</Table.Head>
							<Table.Head class="hidden sm:table-cell">Joined</Table.Head>
							{#if owner}<Table.Head class="text-right"><span class="sr-only">Actions</span></Table.Head>{/if}
						</Table.Row>
					</Table.Header>
					<Table.Body>
						{#each data.members as member (member.account)}
							<Table.Row data-testid="member-row">
								<Table.Cell class="max-w-24 truncate font-medium sm:max-w-none" title={member.email}>{member.email}</Table.Cell>
								<Table.Cell>
									{#if owner}
										<NativeSelect
											size="sm"
											class="min-w-24"
											value={member.role}
											disabled={rowBusy === member.account}
											aria-label="Role for {member.email}"
											data-testid="member-role"
											onchange={(event: Event) =>
												setRole(member, (event.currentTarget as HTMLSelectElement).value as Role)}
										>
											<NativeSelectOption value="owner">Owner</NativeSelectOption>
											<NativeSelectOption value="member">Member</NativeSelectOption>
											<NativeSelectOption value="viewer">Viewer</NativeSelectOption>
										</NativeSelect>
									{:else}
										{ROLE_WORDS[member.role]}
									{/if}
								</Table.Cell>
								<Table.Cell class="hidden text-muted-foreground sm:table-cell">{when(member.created_at)}</Table.Cell>
								{#if owner}
									<Table.Cell class="text-right">
										<Button
											variant="outline"
											size="xs"
											data-testid="member-remove"
											onclick={() => {
												removing = member;
												removeError = null;
												removeOpen = true;
											}}>Remove</Button
										>
									</Table.Cell>
								{/if}
							</Table.Row>
						{/each}
					</Table.Body>
				</Table.Root>
			</Card.Content>
		</Card.Root>

		{#if data.sent.length > 0}
			<Card.Root data-testid="sent-invitations">
				<Card.Header>
					<Card.Title>Waiting for an answer</Card.Title>
					<Card.Description>Invitations sent from this workspace that nobody has accepted yet.</Card.Description>
				</Card.Header>
				<Card.Content class="flex flex-col gap-3">
					{#each data.sent as invitation (invitation.id)}
						<div class="flex flex-wrap items-center justify-between gap-3">
							<p class="text-sm">
								<span class="font-medium">{invitation.email}</span>
								<span class="text-muted-foreground"> · sent {when(invitation.created_at)}</span>
							</p>
							<div class="flex items-center gap-2">
								<Badge variant="pending">{ROLE_WORDS[invitation.role]}</Badge>
								<Button
									variant="outline"
									size="xs"
									disabled={rowBusy === invitation.id}
									data-testid="invitation-withdraw"
									onclick={() => withdraw(invitation.id)}>Withdraw</Button
								>
							</div>
						</div>
					{/each}
				</Card.Content>
			</Card.Root>
		{/if}
	{/if}
</div>

<Confirm
	bind:open={removeOpen}
	title="Remove {removing?.email ?? ''}?"
	confirm="Remove member"
	busy={removeBusy}
	onConfirm={remove}
>
	They lose access to this workspace straight away. Their account and their other workspaces are
	not touched.
	{#if removeError}<span class="mt-2 block text-attention-ink" role="alert">{removeError}</span>{/if}
</Confirm>
