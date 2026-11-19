<script lang="ts">
	import { page } from '$app/state';
	import * as Sidebar from '$lib/components/ui/sidebar/index.js';
	import { Button } from '$lib/components/ui/button/index.js';
	import { logout, session } from '$lib/session.svelte';
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
		{ label: 'Review Rules', href: '/review-rules', icon: RiRulerLine, admin: false },
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
	<Sidebar.Header>
		<span class="px-2 py-1 text-lg font-semibold">Adus</span>
	</Sidebar.Header>
	<Sidebar.Content>
		<Sidebar.Group>
			<Sidebar.Menu>
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
	<Sidebar.Footer>
		<p class="truncate px-2 text-xs text-muted-foreground">{session.user?.email}</p>
		<Button variant="outline" size="sm" onclick={logout}>Log out</Button>
	</Sidebar.Footer>
</Sidebar.Root>
