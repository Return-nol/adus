<script lang="ts">
	import { page } from '$app/state';
	import * as Sidebar from '#lib/components/ui/sidebar/index.js';
	import { Button } from '#lib/components/ui/button/index.js';
	import ThemeToggle from '#lib/components/theme-toggle.svelte';
	import { logout, session } from '#lib/session.svelte.js';
	import RiGitRepositoryLine from 'remixicon-svelte/icons/git-repository-line';
	import RiRobot2Line from 'remixicon-svelte/icons/robot-2-line';
	import RiRulerLine from 'remixicon-svelte/icons/ruler-line';
	import RiGitPullRequestLine from 'remixicon-svelte/icons/git-pull-request-line';
	import RiBarChartLine from 'remixicon-svelte/icons/bar-chart-line';
	import RiGroupLine from 'remixicon-svelte/icons/group-line';
	import RiSettings3Line from 'remixicon-svelte/icons/settings-3-line';
	import RiPulseLine from 'remixicon-svelte/icons/pulse-line';
	import RiUserLine from 'remixicon-svelte/icons/user-line';

	// `admin` items are hidden from role `user`. Some routes arrive in later tasks.
	const items = [
		{ label: 'Repositories', href: '/repositories', icon: RiGitRepositoryLine, admin: false },
		{ label: 'AI Providers', href: '/ai-providers', icon: RiRobot2Line, admin: false },
		{ label: 'Review Rules', href: '/rules', icon: RiRulerLine, admin: false },
		{ label: 'Reviews', href: '/reviews', icon: RiGitPullRequestLine, admin: false },
		{ label: 'Token Usage', href: '/usage', icon: RiBarChartLine, admin: false },
		{ label: 'Users', href: '/users', icon: RiGroupLine, admin: true },
		{ label: 'Settings', href: '/settings', icon: RiSettings3Line, admin: true },
		{ label: 'System status', href: '/system', icon: RiPulseLine, admin: true },
		{ label: 'Profile', href: '/profile', icon: RiUserLine, admin: false }
	];

	const visible = $derived(items.filter((i) => !i.admin || session.user?.role === 'admin'));
</script>

<Sidebar.Root>
	<Sidebar.Header class="px-3 py-3">
		<a href="/repositories" class="flex items-center gap-2.5 px-1">
			<span
				class="flex size-8 shrink-0 items-center justify-center rounded-sm bg-primary font-display text-lg font-semibold text-primary-foreground"
				aria-hidden="true"
			>
				🛀
			</span>
			<div class="flex flex-col leading-none">
				<span class="font-display text-lg font-medium tracking-tight text-sidebar-foreground">
					Adus
				</span>
				<span class="mt-0.5 font-mono text-[11px] text-muted-foreground">AI Code Reviewer</span>
			</div>
		</a>
	</Sidebar.Header>
	<Sidebar.Content>
		<Sidebar.Group>
			<Sidebar.Menu class="gap-0.5">
				{#each visible as item (item.href)}
					<Sidebar.MenuItem>
						<Sidebar.MenuButton isActive={page.url.pathname.startsWith(item.href)}>
							{#snippet child({ props })}
								<a href={item.href} {...props}>
									<item.icon />
									<span>{item.label}</span>
								</a>
							{/snippet}
						</Sidebar.MenuButton>
					</Sidebar.MenuItem>
				{/each}
			</Sidebar.Menu>
		</Sidebar.Group>
	</Sidebar.Content>
	<Sidebar.Footer class="gap-2.5 p-3">
		<ThemeToggle />
		<Sidebar.Separator class="mx-0" />
		<p class="truncate px-1 text-xs text-muted-foreground">{session.user?.email}</p>
		<Button variant="outline" size="sm" class="w-full" onclick={logout}>Log out</Button>
	</Sidebar.Footer>
</Sidebar.Root>
