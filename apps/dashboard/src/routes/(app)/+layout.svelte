<!--
  The app's shell (S2a): a light sidebar — the workspace switcher at its head, grouped
  16px nav, the account menu at its foot — around one continuous page. No frame separates
  the sidebar from the content; the only divider is the top bar's hairline.
-->
<script lang="ts">
	import ActivityIcon from '@lucide/svelte/icons/activity';
	import BookOpenIcon from '@lucide/svelte/icons/book-open';
	import CalendarClockIcon from '@lucide/svelte/icons/calendar-clock';
	import GlobeIcon from '@lucide/svelte/icons/globe';
	import KeyRoundIcon from '@lucide/svelte/icons/key-round';
	import RadarIcon from '@lucide/svelte/icons/radar';
	import RocketIcon from '@lucide/svelte/icons/rocket';
	import SettingsIcon from '@lucide/svelte/icons/settings';
	import ShieldIcon from '@lucide/svelte/icons/shield';
	import SirenIcon from '@lucide/svelte/icons/siren';
	import UsersIcon from '@lucide/svelte/icons/users';
	import WebhookIcon from '@lucide/svelte/icons/webhook';
	import { page } from '$app/state';
	import * as Sidebar from '#lib/components/ui/sidebar/index.js';
	import { guideOffered } from '#lib/guide.svelte.js';
	import { current } from '#lib/workspace.svelte.js';
	import AccountMenu from '#lib/components/shell/AccountMenu.svelte';
	import ThemeToggle from '#lib/components/shell/ThemeToggle.svelte';
	import WorkspaceSwitcher from '#lib/components/shell/WorkspaceSwitcher.svelte';
	import type { LayoutProps } from './$types';

	let { children, data }: LayoutProps = $props();

	const route = $derived(page.route.id ?? '');
	const who = $derived(current());
	const email = $derived(data.account?.email ?? who?.name ?? 'account');
	// The guide is for people setting a workspace up, so viewers and the operator's
	// all-workspaces session don't get the link.
	const guided = $derived(
		(who?.role === 'owner' || who?.role === 'member') && guideOffered(who?.id)
	);
</script>

<Sidebar.Provider>
	<Sidebar.Root collapsible="icon">
		<nav aria-label="Workspace" class="flex min-h-0 flex-1 flex-col">
			<Sidebar.Header>
				<Sidebar.Menu>
					<Sidebar.MenuItem>
						<WorkspaceSwitcher />
					</Sidebar.MenuItem>
				</Sidebar.Menu>
			</Sidebar.Header>

			<Sidebar.Content>
				<Sidebar.Group>
					<Sidebar.GroupLabel>Watch</Sidebar.GroupLabel>
					<Sidebar.Menu>
						{#if guided}
							<Sidebar.MenuItem>
								<Sidebar.MenuButton isActive={route.startsWith('/(app)/onboarding')} tooltipContent="Get started">
									{#snippet child({ props })}
										<a href="/onboarding" {...props} data-testid="nav-guide"><RocketIcon /><span>Get started</span></a>
									{/snippet}
								</Sidebar.MenuButton>
							</Sidebar.MenuItem>
						{/if}
						<Sidebar.MenuItem>
							<Sidebar.MenuButton isActive={route === '/(app)' || route.startsWith('/(app)/monitors')} tooltipContent="Monitors">
								{#snippet child({ props })}
									<a href="/" {...props}><RadarIcon /><span>Monitors</span></a>
								{/snippet}
							</Sidebar.MenuButton>
						</Sidebar.MenuItem>
						<Sidebar.MenuItem>
							<Sidebar.MenuButton isActive={route.startsWith('/(app)/incidents')} tooltipContent="Incidents">
								{#snippet child({ props })}
									<a href="/incidents" {...props}><SirenIcon /><span>Incidents</span></a>
								{/snippet}
							</Sidebar.MenuButton>
						</Sidebar.MenuItem>
						<Sidebar.MenuItem>
							<Sidebar.MenuButton isActive={route.startsWith('/(app)/maintenance')} tooltipContent="Maintenance">
								{#snippet child({ props })}
									<a href="/maintenance" {...props}><CalendarClockIcon /><span>Maintenance</span></a>
								{/snippet}
							</Sidebar.MenuButton>
						</Sidebar.MenuItem>
					</Sidebar.Menu>
				</Sidebar.Group>

				<Sidebar.Group>
					<Sidebar.GroupLabel>Workspace</Sidebar.GroupLabel>
					<Sidebar.Menu>
						<Sidebar.MenuItem>
							<Sidebar.MenuButton isActive={route.startsWith('/(app)/settings/general')} tooltipContent="General">
								{#snippet child({ props })}
									<a href="/settings/general" {...props}><SettingsIcon /><span>General</span></a>
								{/snippet}
							</Sidebar.MenuButton>
						</Sidebar.MenuItem>
						<Sidebar.MenuItem>
							<Sidebar.MenuButton isActive={route.startsWith('/(app)/settings/members')} tooltipContent="Members">
								{#snippet child({ props })}
									<a href="/settings/members" {...props}><UsersIcon /><span>Members</span></a>
								{/snippet}
							</Sidebar.MenuButton>
						</Sidebar.MenuItem>
						<Sidebar.MenuItem>
							<Sidebar.MenuButton isActive={route.startsWith('/(app)/settings/tokens')} tooltipContent="Tokens">
								{#snippet child({ props })}
									<a href="/settings/tokens" {...props}><KeyRoundIcon /><span>Tokens</span></a>
								{/snippet}
							</Sidebar.MenuButton>
						</Sidebar.MenuItem>
						<Sidebar.MenuItem>
							<Sidebar.MenuButton isActive={route.startsWith('/(app)/settings/alerts')} tooltipContent="Alerts">
								{#snippet child({ props })}
									<a href="/settings/alerts" {...props}><WebhookIcon /><span>Alerts</span></a>
								{/snippet}
							</Sidebar.MenuButton>
						</Sidebar.MenuItem>
						<Sidebar.MenuItem>
							<Sidebar.MenuButton isActive={route.startsWith('/(app)/settings/status-page')} tooltipContent="Status page">
								{#snippet child({ props })}
									<a href="/settings/status-page" {...props}><GlobeIcon /><span>Status page</span></a>
								{/snippet}
							</Sidebar.MenuButton>
						</Sidebar.MenuItem>
					</Sidebar.Menu>
				</Sidebar.Group>

				{#if who?.role === 'operator'}
					<Sidebar.Group>
						<Sidebar.GroupLabel>Server</Sidebar.GroupLabel>
						<Sidebar.Menu>
							<Sidebar.MenuItem>
								<Sidebar.MenuButton isActive={route.startsWith('/(app)/operator')} tooltipContent="Operator">
									{#snippet child({ props })}
										<a href="/operator" {...props} data-testid="nav-operator"><ShieldIcon /><span>Operator</span></a>
									{/snippet}
								</Sidebar.MenuButton>
							</Sidebar.MenuItem>
						</Sidebar.Menu>
					</Sidebar.Group>
				{/if}

				<Sidebar.Group>
					<Sidebar.GroupLabel>Monitor</Sidebar.GroupLabel>
					<Sidebar.Menu>
						<Sidebar.MenuItem>
							<Sidebar.MenuButton isActive={route.startsWith('/(app)/status')} tooltipContent="Status">
								{#snippet child({ props })}
									<a href="/status" {...props}><ActivityIcon /><span>Status</span></a>
								{/snippet}
							</Sidebar.MenuButton>
						</Sidebar.MenuItem>
						<Sidebar.MenuItem>
							<Sidebar.MenuButton isActive={route.startsWith('/(app)/guide')} tooltipContent="Guide">
								{#snippet child({ props })}
									<a href="/guide" {...props}><BookOpenIcon /><span>Guide</span></a>
								{/snippet}
							</Sidebar.MenuButton>
						</Sidebar.MenuItem>
					</Sidebar.Menu>
				</Sidebar.Group>
			</Sidebar.Content>

			<Sidebar.Footer>
				<Sidebar.Menu>
					<Sidebar.MenuItem>
						{#if who}
							<div class="flex items-center justify-between gap-2 px-1 pb-1">
								<span class="min-w-0 truncate text-xs text-muted-foreground" title={email}>{email}</span>
								<AccountMenu {email} role={who.role} />
							</div>
						{/if}
					</Sidebar.MenuItem>
				</Sidebar.Menu>
			</Sidebar.Footer>
			<Sidebar.Rail />
		</nav>
	</Sidebar.Root>

	<Sidebar.Inset class="min-w-0">
		<header class="sticky top-0 z-10 flex h-14 shrink-0 items-center gap-2 border-b border-border bg-background/90 px-4 backdrop-blur">
			<Sidebar.Trigger class="-ml-1 mr-1" aria-label="Toggle sidebar" />
			<div class="ml-auto flex items-center gap-1">
				<ThemeToggle />
			</div>
		</header>
		{#if data.invited > 0 && !route.startsWith('/(app)/settings/members')}
			<a
				href="/settings/members"
				class="border-b border-border bg-brand-soft px-5 py-2.5 text-sm text-brand-ink hover:underline sm:px-8 lg:px-10"
				data-testid="invited-banner"
			>
				{data.invited === 1
					? 'You have been invited to another workspace.'
					: `You have been invited to ${data.invited} other workspaces.`}
				Review the invitation
			</a>
		{/if}
		{#key page.url.pathname}
			<div class="page-enter mx-auto w-full max-w-6xl flex-1 px-5 py-8 sm:px-8 lg:px-10">
				{@render children()}
			</div>
		{/key}
	</Sidebar.Inset>
</Sidebar.Provider>
