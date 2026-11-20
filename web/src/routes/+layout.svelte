<script lang="ts">
	import './layout.css';
	import { onMount } from 'svelte';
	import { page } from '$app/state';
	import { ModeWatcher } from 'mode-watcher';
	import favicon from '#lib/assets/favicon.svg';
	import AppSidebar from '#lib/components/app-sidebar.svelte';
	import ThemeToggle from '#lib/components/theme-toggle.svelte';
	import { Button } from '#lib/components/ui/button/index.js';
	import * as Sidebar from '#lib/components/ui/sidebar/index.js';
	import { Toaster } from '#lib/components/ui/sonner/index.js';
	import { loadSession, session } from '#lib/session.svelte.js';
	import type { LayoutProps } from './$types';

	let { children }: LayoutProps = $props();

	// Pages shown without the sidebar.
	const isPublic = $derived(['/login', '/setup'].includes(page.url.pathname));

	onMount(loadSession);
</script>

<svelte:head>
	<link rel="icon" href={favicon} />
	<title>Adus</title>
</svelte:head>

<ModeWatcher disableHeadScriptInjection themeColors={{ light: '#fffefb', dark: '#201515' }} />
<Toaster />

{#if isPublic}
	<div class="fixed top-4 right-4 z-20">
		<ThemeToggle compact />
	</div>
	{@render children()}
{:else if !session.ready}
	<div class="flex min-h-screen items-center justify-center p-6 text-sm text-muted-foreground">
		Loading...
	</div>
{:else if session.error}
	<div class="flex min-h-screen items-center justify-center p-6">
		<div
			class="grid w-full max-w-sm gap-4 rounded-xl border border-border bg-card p-6 text-card-foreground"
		>
			<p class="text-sm">Cannot reach the server.</p>
			<Button class="w-fit" onclick={loadSession}>Retry</Button>
		</div>
	</div>
{:else if session.user}
	<Sidebar.Provider>
		<AppSidebar />
		<div class="flex min-h-screen flex-1 flex-col">
			<header
				class="flex h-14 items-center gap-3 border-b border-border/70 bg-background px-4 md:hidden"
			>
				<Sidebar.Trigger />
				<span class="font-display text-lg font-medium tracking-tight">Adus</span>
			</header>
			<main class="mx-auto w-full max-w-6xl flex-1 px-5 py-6 sm:px-8 sm:py-8">
				{@render children()}
			</main>
		</div>
	</Sidebar.Provider>
{/if}
