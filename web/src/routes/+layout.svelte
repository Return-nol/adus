<script lang="ts">
	import './layout.css';
	import { onMount } from 'svelte';
	import { page } from '$app/state';
	import favicon from '#lib/assets/favicon.svg';
	import AppSidebar from '$lib/components/app-sidebar.svelte';
	import { Button } from '$lib/components/ui/button/index.js';
	import * as Sidebar from '$lib/components/ui/sidebar/index.js';
	import { Toaster } from '$lib/components/ui/sonner/index.js';
	import { loadSession, session } from '$lib/session.svelte';
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

<Toaster richColors />

{#if isPublic}
	{@render children()}
{:else if !session.ready}
	<p class="p-6 text-muted-foreground">Loading...</p>
{:else if session.error}
	<div class="grid gap-4 p-6">
		<p>Cannot reach the server.</p>
		<Button class="w-fit" onclick={loadSession}>Retry</Button>
	</div>
{:else if session.user}
	<Sidebar.Provider>
		<AppSidebar />
		<main class="flex-1 p-6">
			<Sidebar.Trigger class="mb-4 md:hidden" />
			{@render children()}
		</main>
	</Sidebar.Provider>
{/if}
