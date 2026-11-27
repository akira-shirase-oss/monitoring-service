<!--
  The workspace picker at the sidebar's head (S2): what the API calls "the workspace" for
  every scoped request. Choosing another writes the pick (workspace.svelte.ts) and reloads
  the data, because every screen answers for one workspace at a time — a header value, not
  a filter. The list itself comes from GET /api/workspaces: the memberships the account
  actually holds, never something typed or guessed.
-->
<script lang="ts">
	import ChevronsUpDownIcon from '@lucide/svelte/icons/chevrons-up-down';
	import { invalidateAll } from '$app/navigation';
	import * as DropdownMenu from '#lib/components/ui/dropdown-menu/index.js';
	import * as Sidebar from '#lib/components/ui/sidebar/index.js';
	import { current, pick, workspace } from '#lib/workspace.svelte.js';
	import Mark from './Mark.svelte';

	const shown = $derived(current());

	async function choose(id: string) {
		if (id === workspace.picked) return;
		// Hold the page still until the picked workspace's data is on it (see `switching`).
		workspace.switching = true;
		pick(id);
		try {
			await invalidateAll();
		} finally {
			workspace.switching = false;
		}
	}
</script>

<DropdownMenu.Root>
	<DropdownMenu.Trigger>
		{#snippet child({ props })}
			<Sidebar.MenuButton size="lg" {...props} class="data-[state=open]:bg-sidebar-accent" data-testid="workspace-switcher">
				<Mark size={28} />
				<span class="grid min-w-0 flex-1 text-left leading-tight">
					<span class="truncate font-medium" data-testid="workspace-name">{shown?.name ?? '…'}</span>
					<span class="truncate text-xs text-muted-foreground">{shown ? `${shown.slug} · ${shown.role}` : ''}</span>
				</span>
				<ChevronsUpDownIcon class="ml-auto text-muted-foreground" aria-hidden="true" />
				<span class="sr-only">: switch workspace</span>
			</Sidebar.MenuButton>
		{/snippet}
	</DropdownMenu.Trigger>
	<DropdownMenu.Content class="w-64" align="start">
		<DropdownMenu.Label class="text-xs text-muted-foreground">Workspaces</DropdownMenu.Label>
		{#each workspace.list as member (member.id)}
			<DropdownMenu.Item onSelect={() => choose(member.id)} data-testid="workspace-option">
				<span class="flex size-6 items-center justify-center rounded-md border border-border text-xs font-semibold"
					>{member.name.slice(0, 1).toUpperCase()}</span
				>
				<span class="min-w-0 flex-1">
					<span class="block truncate">{member.name}</span>
					<span class="block truncate text-xs text-muted-foreground">{member.role}</span>
				</span>
				{#if member.id === shown?.id}<span class="ml-auto text-xs text-muted-foreground">current</span>{/if}
			</DropdownMenu.Item>
		{/each}
	</DropdownMenu.Content>
</DropdownMenu.Root>
