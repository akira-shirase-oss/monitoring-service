<!--
  The server operator's own page: the two hosted-product jobs that belong to nobody's
  workspace — handing out sign-up codes, and moving a workspace between plans. It reads
  the membership list the shell already loaded; for the operator that is every workspace.
-->
<script lang="ts">
	import CheckIcon from '@lucide/svelte/icons/check';
	import CopyIcon from '@lucide/svelte/icons/copy';
	import { invalidateAll } from '$app/navigation';
	import { Button } from '#lib/components/ui/button/index.js';
	import * as Card from '#lib/components/ui/card/index.js';
	import { Input } from '#lib/components/ui/input/index.js';
	import { NativeSelect, NativeSelectOption } from '#lib/components/ui/native-select/index.js';
	import * as Table from '#lib/components/ui/table/index.js';
	import Confirm from '#lib/components/shell/Confirm.svelte';
	import PageHeader from '#lib/components/shell/PageHeader.svelte';
	import Problem from '#lib/components/shell/Problem.svelte';
	import { call } from '#lib/call.js';
	import { when } from '#lib/time.js';
	import { current, workspace, type Membership } from '#lib/workspace.svelte.js';

	const operator = $derived(current()?.role === 'operator');

	let ttl = $state('72');
	let minting = $state(false);
	let mintError = $state<string | null>(null);
	// Shown until the next code replaces it or the page is left; only a digest is stored.
	let invite = $state<{ code: string; expires_at: string } | null>(null);
	let copied = $state(false);

	let planBusy = $state<string | null>(null);
	let planError = $state<string | null>(null);

	async function mint(event: SubmitEvent) {
		event.preventDefault();
		minting = true;
		mintError = null;
		const answer = await call<{ invite: { code: string; expires_at: string } }>('/api/invites', {
			method: 'POST',
			body: { ttl_hours: Number(ttl) }
		});
		minting = false;
		if (!answer.ok) {
			mintError = answer.failure.message;
			return;
		}
		invite = answer.body.invite;
		copied = false;
	}

	async function copy() {
		if (!invite) return;
		try {
			await navigator.clipboard.writeText(invite.code);
			copied = true;
		} catch {
			copied = false;
		}
	}

	// Same rule as a role: the select proposes, a confirmation decides.
	let moving = $state<{ row: Membership; plan: string } | null>(null);
	let moveOpen = $state(false);

	// Bumped on every proposal so the selects are drawn again from the stored plans.
	let drawn = $state(0);

	function askMove(row: Membership, select: HTMLSelectElement) {
		const plan = select.value;
		drawn += 1;
		if (plan === (row.plan ?? 'free')) return;
		moving = { row, plan };
		planError = null;
		moveOpen = true;
	}

	async function move() {
		if (moving === null) return;
		planBusy = moving.row.id;
		planError = null;
		const answer = await call(`/api/workspaces/${moving.row.id}/plan`, {
			method: 'PATCH',
			body: { plan: moving.plan }
		});
		planBusy = null;
		if (!answer.ok) {
			planError = answer.failure.message;
			return;
		}
		moveOpen = false;
		moving = null;
		await invalidateAll();
	}
</script>

<svelte:head><title>Operator · Service Monitor</title></svelte:head>

<PageHeader title="Operator">
	{#snippet description()}
		Jobs that belong to the server, not to one workspace: sign-up codes and plans.
	{/snippet}
</PageHeader>

<div class="flex flex-col gap-6">
	{#if !operator}
		<Problem error={{ status: 403, message: 'This page is for the server operator. Sign in with the operator token to use it.' }} />
	{:else}
		<Card.Root>
			<Card.Header>
				<Card.Title>Sign-up codes</Card.Title>
				<Card.Description>
					Sign-up is by invitation. Each code creates one account and its workspace, then stops
					working.
				</Card.Description>
			</Card.Header>
			<Card.Content>
				<form class="flex flex-col gap-3 sm:flex-row sm:items-end" onsubmit={mint}>
					<div class="space-y-1.5">
						<label for="invite-ttl" class="text-sm font-medium">Valid for</label>
						<NativeSelect id="invite-ttl" bind:value={ttl} data-testid="invite-ttl">
							<NativeSelectOption value="24">1 day</NativeSelectOption>
							<NativeSelectOption value="72">3 days</NativeSelectOption>
							<NativeSelectOption value="168">7 days</NativeSelectOption>
							<NativeSelectOption value="720">30 days</NativeSelectOption>
						</NativeSelect>
					</div>
					<Button type="submit" disabled={minting} data-testid="invite-create">
						{minting ? 'Creating…' : 'Create code'}
					</Button>
				</form>
				{#if mintError}
					<p class="mt-3 text-sm text-attention-ink" role="alert">{mintError}</p>
				{/if}
				{#if invite}
					<div class="mt-4 space-y-2" data-testid="invite-result">
						<div class="flex items-center gap-2">
							<Input
								readonly
								value={invite.code}
								class="font-mono text-sm"
								aria-label="Sign-up code"
								data-testid="invite-code"
								onfocus={(event: FocusEvent) => (event.currentTarget as HTMLInputElement).select()}
							/>
							<Button variant="outline" onclick={copy}>
								{#if copied}<CheckIcon /> Copied{:else}<CopyIcon /> Copy{/if}
							</Button>
						</div>
						<p class="text-sm text-muted-foreground">
							Send it with the sign-up address,
							<code class="font-mono text-xs">{location.origin}/signup</code>. It works until
							{when(invite.expires_at)} and can't be shown again after you leave this page.
						</p>
					</div>
				{/if}
			</Card.Content>
		</Card.Root>

		<Card.Root>
			<Card.Header>
				<Card.Title>Workspaces</Card.Title>
				<Card.Description>
					A plan change applies to the workspace's next write. Nothing already running is paused
					or deleted.
				</Card.Description>
			</Card.Header>
			<Card.Content class="p-0">
				{#if planError}
					<p class="px-6 pb-3 text-sm text-attention-ink" role="alert">{planError}</p>
				{/if}
				<Table.Root data-testid="workspaces-table">
					<Table.Header>
						<Table.Row>
							<Table.Head>Name</Table.Head>
							<Table.Head>Address</Table.Head>
							<Table.Head>Plan</Table.Head>
						</Table.Row>
					</Table.Header>
					<Table.Body>
						{#each workspace.list as row (row.id)}
							<Table.Row data-testid="workspace-row">
								<Table.Cell class="font-medium">{row.name}</Table.Cell>
								<Table.Cell class="font-mono text-xs text-muted-foreground">{row.slug}</Table.Cell>
								<Table.Cell>
									{#key drawn}
									<NativeSelect
										size="sm"
										value={row.plan ?? 'free'}
										disabled={planBusy === row.id}
										aria-label="Plan for {row.name}"
										data-testid="workspace-plan"
										onchange={(event: Event) => askMove(row, event.currentTarget as HTMLSelectElement)}
									>
										<NativeSelectOption value="free">free</NativeSelectOption>
										<NativeSelectOption value="operator">operator (no plan limits)</NativeSelectOption>
									</NativeSelect>
									{/key}
								</Table.Cell>
							</Table.Row>
						{/each}
					</Table.Body>
				</Table.Root>
			</Card.Content>
		</Card.Root>
	{/if}
</div>

<Confirm
	bind:open={moveOpen}
	title="Move {moving?.row.name ?? ''} to the {moving?.plan ?? ''} plan?"
	confirm="Change plan"
	busy={planBusy !== null}
	onConfirm={move}
>
	{#if moving?.plan === 'free'}
		Its next write is held to the free plan's limits. Nothing already running is paused or
		deleted.
	{:else}
		The plan's own limits are lifted; the server's ceilings still apply.
	{/if}
	{#if planError}<span class="mt-2 block text-attention-ink" role="alert">{planError}</span>{/if}
</Confirm>
