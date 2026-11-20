<script lang="ts">
	import { toast } from 'svelte-sonner';
	import { api, ApiError } from '#lib/api.js';
	import type { Connection } from '#lib/components/connection-dialog.svelte';
	import { Button } from '#lib/components/ui/button/index.js';
	import * as Dialog from '#lib/components/ui/dialog/index.js';
	import { Input } from '#lib/components/ui/input/index.js';

	type RemoteRepo = {
		ref: { external_id: string; owner: string; project: string | null; name: string };
		default_branch: string | null;
		added: boolean;
		display_name: string;
	};

	let {
		open = $bindable(false),
		connection,
		onadded
	}: { open: boolean; connection: Connection | null; onadded: () => void } = $props();

	let remote = $state<RemoteRepo[]>([]);
	let picked = $state<string[]>([]);
	let loading = $state(false);
	let busy = $state(false);
	let loadError = $state('');
	let query = $state('');
	let failures = $state<string[]>([]);

	// Only the view is filtered; `picked` keeps checked repos that scroll out of the results.
	const sorted = $derived([...remote].sort((a, b) => a.display_name.localeCompare(b.display_name)));
	const shown = $derived(
		sorted.filter((r) => r.display_name.toLowerCase().includes(query.trim().toLowerCase()))
	);

	async function loadRemote() {
		if (!connection) return;
		loading = true;
		loadError = '';
		try {
			remote = await api<RemoteRepo[]>('GET', `/connections/${connection.id}/remote-repos`);
		} catch (e) {
			remote = [];
			loadError = e instanceof ApiError ? e.message : 'Could not reach the server';
		} finally {
			loading = false;
		}
	}

	$effect(() => {
		if (!open) return;
		picked = [];
		failures = [];
		query = '';
		loadRemote();
	});

	// Adds one by one so a failing repo (e.g. webhook_failed) does not block the others.
	async function addPicked() {
		if (!connection) return;
		busy = true;
		failures = [];
		let added = 0;
		for (const id of picked) {
			const repo = remote.find((r) => r.ref.external_id === id);
			try {
				await api('POST', '/repositories', { connection_id: connection.id, external_id: id });
				added++;
			} catch (e) {
				const why = e instanceof ApiError ? e.message : 'Could not reach the server';
				failures.push(`${repo?.display_name ?? id}: ${why}`);
			}
		}
		busy = false;
		picked = [];
		if (added > 0) {
			toast.success(`Added ${added} repositor${added === 1 ? 'y' : 'ies'}`);
			onadded();
		}
		if (failures.length === 0) open = false;
		else await loadRemote();
	}
</script>

<Dialog.Root bind:open>
	<Dialog.Content>
		<Dialog.Header>
			<Dialog.Title>Add repositories from {connection?.name}</Dialog.Title>
			<Dialog.Description>A webhook is registered for each repository you add.</Dialog.Description>
		</Dialog.Header>
		{#if loading}
			<p class="text-sm text-muted-foreground">Loading repositories...</p>
		{:else if loadError}
			<p class="text-sm text-destructive" role="alert">{loadError}</p>
		{:else}
			<Input type="search" placeholder="Search repositories" bind:value={query} />
			<p class="text-sm text-muted-foreground">
				{shown.length} of {remote.length} repositories
			</p>
			<ul class="max-h-72 space-y-2 overflow-y-auto">
				{#each shown as repo (repo.ref.external_id)}
					<li>
						<label class="flex items-center gap-2 text-sm">
							<input
								type="checkbox"
								class="size-4 rounded-sm accent-primary"
								disabled={repo.added || busy}
								value={repo.ref.external_id}
								bind:group={picked}
							/>
							<span class={repo.added ? 'text-muted-foreground' : ''}>
								{repo.display_name}{repo.added ? ' (added)' : ''}
							</span>
						</label>
					</li>
				{:else}
					<li class="text-sm text-muted-foreground">
						{remote.length ? 'No repositories match' : 'No repositories visible to this token.'}
					</li>
				{/each}
			</ul>
		{/if}
		{#if failures.length}
			<ul class="space-y-1 text-sm text-destructive" role="alert">
				{#each failures as failure}<li>{failure}</li>{/each}
			</ul>
		{/if}
		<Dialog.Footer>
			<Button disabled={busy || picked.length === 0} onclick={addPicked}>
				Add {picked.length || ''} selected
			</Button>
		</Dialog.Footer>
	</Dialog.Content>
</Dialog.Root>
