<script lang="ts">
	import { toast } from 'svelte-sonner';
	import { api, ApiError } from '#lib/api.js';
	import type { Connection } from '#lib/components/connection-dialog.svelte';
	import { Button } from '#lib/components/ui/button/index.js';
	import * as Dialog from '#lib/components/ui/dialog/index.js';
	import * as Popover from '#lib/components/ui/popover/index.js';
	import * as Command from '#lib/components/ui/command/index.js';
	import { Badge } from '#lib/components/ui/badge/index.js';
	import RiArrowDownSLine from 'remixicon-svelte/icons/arrow-down-s-line';
	import RiCloseLine from 'remixicon-svelte/icons/close-line';
	import RiLoader3Line from 'remixicon-svelte/icons/loader-3-line';

	type RemoteRepo = {
		ref: { external_id: string; owner: string; project: string | null; name: string };
		default_branch: string | null;
		added: boolean;
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
	let failures = $state<string[]>([]);
	let pickerOpen = $state(false);

	const label = (repo: RemoteRepo) => `${repo.ref.owner}/${repo.ref.name}`;
	const nameOf = (id: string) => {
		const repo = remote.find((r) => r.ref.external_id === id);
		return repo ? label(repo) : id;
	};

	function toggle(id: string) {
		picked = picked.includes(id) ? picked.filter((p) => p !== id) : [...picked, id];
	}

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
		pickerOpen = false;
		loadRemote();
	});

	// Adds one by one so a failing repo (e.g. webhook_failed) does not block the others.
	async function addPicked() {
		if (!connection) return;
		busy = true;
		pickerOpen = false;
		failures = [];
		let added = 0;
		for (const id of picked) {
			const repo = remote.find((r) => r.ref.external_id === id);
			try {
				await api('POST', '/repositories', { connection_id: connection.id, external_id: id });
				added++;
			} catch (e) {
				const why = e instanceof ApiError ? e.message : 'Could not reach the server';
				failures.push(`${repo?.ref.name ?? id}: ${why}`);
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
			<p class="flex min-h-4 items-center justify-center gap-2 py-2 text-sm text-muted-foreground">
				<RiLoader3Line class="size-4 animate-spin" aria-hidden="true" />
				Loading repositories...
			</p>
		{:else if loadError}
			<p class="text-sm text-destructive" role="alert">{loadError}</p>
		{:else}
			<Popover.Root bind:open={pickerOpen}>
				<Popover.Trigger disabled={busy}>
					{#snippet child({ props })}
						<Button
							{...props}
							variant="outline"
							role="combobox"
							aria-expanded={pickerOpen}
							class="w-full justify-between rounded-sm font-normal"
						>
							{#if picked.length}
								{picked.length} repositor{picked.length === 1 ? 'y' : 'ies'} selected
							{:else}
								<span class="text-muted-foreground">Select repositories...</span>
							{/if}
							<RiArrowDownSLine data-icon="inline-end" class="text-muted-foreground" />
						</Button>
					{/snippet}
				</Popover.Trigger>
				<Popover.Content class="w-(--bits-popover-anchor-width) p-0" align="start">
					<Command.Root>
						<Command.Input placeholder="Search repositories..." />
						<Command.List>
							<Command.Empty>
								{remote.length ? 'No repository found.' : 'No repositories visible to this token.'}
							</Command.Empty>
							{#each remote as repo (repo.ref.external_id)}
								<Command.Item
									value={label(repo)}
									disabled={repo.added}
									data-checked={picked.includes(repo.ref.external_id)}
									onSelect={() => toggle(repo.ref.external_id)}
								>
									<span class="truncate">{label(repo)}</span>
									{#if repo.added}
										<span class="text-xs text-muted-foreground">added</span>
									{/if}
								</Command.Item>
							{/each}
						</Command.List>
					</Command.Root>
				</Popover.Content>
			</Popover.Root>
			{#if picked.length}
				<div class="flex max-h-32 flex-wrap gap-1.5 overflow-y-auto">
					{#each picked as id (id)}
						<Badge variant="secondary" class="max-w-full">
							<span class="truncate">{nameOf(id)}</span>
							<button
								type="button"
								class="rounded-full hover:text-foreground disabled:opacity-50"
								aria-label="Remove {nameOf(id)}"
								disabled={busy}
								onclick={() => toggle(id)}
							>
								<RiCloseLine class="size-3" />
							</button>
						</Badge>
					{/each}
				</div>
			{/if}
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
