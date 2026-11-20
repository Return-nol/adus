<script lang="ts">
	import { onMount } from 'svelte';
	import { SvelteSet } from 'svelte/reactivity';
	import { toast } from 'svelte-sonner';
	import { api, ApiError } from '#lib/api.js';
	import MemoryDialog, { type Memory } from '#lib/components/memory-dialog.svelte';
	import RuleDialog, {
		severityLabel,
		type Rule,
		type Severity
	} from '#lib/components/rule-dialog.svelte';
	import * as AlertDialog from '#lib/components/ui/alert-dialog/index.js';
	import { Badge } from '#lib/components/ui/badge/index.js';
	import { Button } from '#lib/components/ui/button/index.js';
	import * as Select from '#lib/components/ui/select/index.js';
	import { Switch } from '#lib/components/ui/switch/index.js';
	import * as Table from '#lib/components/ui/table/index.js';
	import * as Tabs from '#lib/components/ui/tabs/index.js';
	import * as Tooltip from '#lib/components/ui/tooltip/index.js';
	import { safeHttpUrl } from '#lib/utils.js';

	type Repo = { id: string; display_name: string };

	const GLOBAL = 'global';
	const MEMORY_PREVIEW = 120;
	const severityVariant: Record<Severity, 'secondary' | 'default' | 'destructive'> = {
		info: 'secondary',
		warning: 'default',
		critical: 'destructive'
	};

	let repos = $state<Repo[]>([]);
	let scope = $state(GLOBAL);
	let rules = $state<Rule[]>([]);
	let memories = $state<Memory[]>([]);
	let loaded = $state(false);
	let loadError = $state(false);
	// Ids (rule, memory) with a request in flight; their controls are disabled.
	const busy = new SvelteSet<string>();

	let ruleOpen = $state(false);
	let editRule = $state<Rule | null>(null);
	let memoryOpen = $state(false);
	let editMemory = $state<Memory | null>(null);
	let confirmOpen = $state(false);
	let confirm = $state<{ title: string; text: string; run: () => Promise<void> } | null>(null);

	const repositoryId = $derived(scope === GLOBAL ? null : scope);
	const scopeLabel = $derived.by(() => {
		const r = repos.find((r) => r.id === scope);
		return r ? r.display_name : 'Global';
	});

	function fail(e: unknown) {
		toast.error(e instanceof ApiError ? e.message : 'Could not reach the server');
	}

	// Only the newest request may write results, so a slow response for a previous scope is dropped.
	let seq = 0;
	async function load() {
		const mine = ++seq;
		loadError = false;
		const q = repositoryId ? `?repository_id=${repositoryId}` : '';
		try {
			const [r, m] = await Promise.all([
				api<Rule[]>('GET', `/rules${q}`),
				api<Memory[]>('GET', `/memories${q}`)
			]);
			if (mine !== seq) return;
			rules = r;
			memories = m;
		} catch (e) {
			if (mine === seq) {
				loadError = true;
				fail(e);
			}
		}
		if (mine === seq) loaded = true;
	}

	onMount(async () => {
		try {
			repos = await api<Repo[]>('GET', '/repositories');
		} catch (e) {
			fail(e);
		}
	});

	// Reload whenever the scope changes (also runs once on mount).
	$effect(() => {
		void scope;
		loaded = false;
		rules = [];
		memories = [];
		load();
	});

	/** Runs `fn` with `id` marked busy, then reloads (which also snaps a failed switch back). */
	async function guarded(id: string, fn: () => Promise<unknown>) {
		if (busy.has(id)) return;
		busy.add(id);
		try {
			await fn();
		} catch (e) {
			fail(e);
		}
		await load();
		busy.delete(id);
	}

	function askDelete(id: string, title: string, path: string) {
		confirm = {
			title,
			text: 'This cannot be undone.',
			run: () => guarded(id, () => api('DELETE', path))
		};
		confirmOpen = true;
	}
</script>

<div class="mb-6 flex flex-wrap items-center justify-between gap-3">
	<h1 class="font-display text-3xl font-medium tracking-[0.01em] text-foreground">Review Rules</h1>
	<Select.Root type="single" bind:value={scope}>
		<Select.Trigger class="w-64" aria-label="Scope">{scopeLabel}</Select.Trigger>
		<Select.Content>
			<Select.Item value={GLOBAL} label="Global">Global</Select.Item>
			{#each repos as r (r.id)}
				<Select.Item value={r.id} label={r.display_name}>{r.display_name}</Select.Item>
			{/each}
		</Select.Content>
	</Select.Root>
</div>

{#snippet loadFailed()}
	<div
		class="rounded-xl border border-dashed border-border bg-card px-8 py-12 text-center text-sm text-muted-foreground"
	>
		Could not load this scope.
		<Button variant="outline" size="sm" class="ml-2" onclick={load}>Retry</Button>
	</div>
{/snippet}

<Tabs.Root value="rules">
	<Tabs.List>
		<Tabs.Trigger value="rules">Rules</Tabs.Trigger>
		<Tabs.Trigger value="memory">Memory</Tabs.Trigger>
	</Tabs.List>

	<Tabs.Content value="rules" class="space-y-4">
		<div class="flex justify-end">
			<Button
				onclick={() => {
					editRule = null;
					ruleOpen = true;
				}}
			>
				Add rule
			</Button>
		</div>
		{#if loadError}
			{@render loadFailed()}
		{:else if loaded && rules.length === 0}
			<div
				class="rounded-xl border border-dashed border-border bg-card px-8 py-12 text-center text-sm text-muted-foreground"
			>
				No rules for this scope yet. Rules tell the reviewer what to look for.
			</div>
		{:else if rules.length > 0}
			<Table.Root>
				<Table.Header>
					<Table.Row>
						<Table.Head>Title</Table.Head>
						<Table.Head>Path glob</Table.Head>
						<Table.Head>Severity</Table.Head>
						<Table.Head>Enabled</Table.Head>
						<Table.Head class="text-right">Actions</Table.Head>
					</Table.Row>
				</Table.Header>
				<Table.Body>
					{#each rules as r (r.id)}
						<Table.Row>
							<Table.Cell class="font-medium">{r.title}</Table.Cell>
							<Table.Cell>
								{#if r.path_glob}<code>{r.path_glob}</code>{:else}
									<span class="text-muted-foreground">all files</span>
								{/if}
							</Table.Cell>
							<Table.Cell>
								<Badge variant={severityVariant[r.severity]}>{severityLabel[r.severity]}</Badge>
							</Table.Cell>
							<Table.Cell>
								<Switch
									aria-label={`Enabled: ${r.title}`}
									disabled={busy.has(r.id)}
									bind:checked={
										() => r.enabled,
										(v) => guarded(r.id, () => api('PATCH', `/rules/${r.id}`, { enabled: v }))
									}
								/>
							</Table.Cell>
							<Table.Cell class="space-x-2 text-right">
								<Button
									variant="outline"
									size="sm"
									disabled={busy.has(r.id)}
									onclick={() => {
										editRule = r;
										ruleOpen = true;
									}}
								>
									Edit
								</Button>
								<Button
									variant="outline"
									size="sm"
									disabled={busy.has(r.id)}
									onclick={() => askDelete(r.id, `Delete rule ${r.title}?`, `/rules/${r.id}`)}
								>
									Delete
								</Button>
							</Table.Cell>
						</Table.Row>
					{/each}
				</Table.Body>
			</Table.Root>
		{/if}
	</Tabs.Content>

	<Tabs.Content value="memory" class="space-y-4">
		<div class="flex justify-end">
			<Button
				onclick={() => {
					editMemory = null;
					memoryOpen = true;
				}}
			>
				Add memory
			</Button>
		</div>
		{#if loadError}
			{@render loadFailed()}
		{:else if loaded && memories.length === 0}
			<div
				class="rounded-xl border border-dashed border-border bg-card px-8 py-12 text-center text-sm text-muted-foreground"
			>
				No memories for this scope yet. Add one here, or comment <code>@adudu remember: …</code> on a
				pull request.
			</div>
		{:else if memories.length > 0}
			<Table.Root>
				<Table.Header>
					<Table.Row>
						<Table.Head>Content</Table.Head>
						<Table.Head>Source</Table.Head>
						<Table.Head>Created by</Table.Head>
						<Table.Head>Enabled</Table.Head>
						<Table.Head class="text-right">Actions</Table.Head>
					</Table.Row>
				</Table.Header>
				<Table.Body>
					{#each memories as m (m.id)}
						<Table.Row>
							<Table.Cell class="max-w-md whitespace-normal">
								{#if m.content.length > MEMORY_PREVIEW}
									<Tooltip.Root>
										<Tooltip.Trigger class="text-left" title={m.content}>
											{m.content.slice(0, MEMORY_PREVIEW)}…
										</Tooltip.Trigger>
										<Tooltip.Content class="max-w-sm whitespace-pre-wrap">
											{m.content}
										</Tooltip.Content>
									</Tooltip.Root>
								{:else}
									{m.content}
								{/if}
							</Table.Cell>
							<Table.Cell>
								<Badge variant="secondary">{m.source === 'manual' ? 'manual' : 'PR comment'}</Badge>
								{#if safeHttpUrl(m.source_url)}
									<a
										href={safeHttpUrl(m.source_url)}
										target="_blank"
										rel="noopener noreferrer"
										class="ml-1 text-sm text-primary underline-offset-4 hover:underline"
									>
										view
									</a>
								{/if}
							</Table.Cell>
							<Table.Cell class="text-muted-foreground">{m.created_by?.email ?? '—'}</Table.Cell>
							<Table.Cell>
								<Switch
									aria-label="Enabled"
									disabled={busy.has(m.id)}
									bind:checked={
										() => m.enabled,
										(v) => guarded(m.id, () => api('PATCH', `/memories/${m.id}`, { enabled: v }))
									}
								/>
							</Table.Cell>
							<Table.Cell class="space-x-2 text-right">
								<Button
									variant="outline"
									size="sm"
									disabled={busy.has(m.id)}
									onclick={() => {
										editMemory = m;
										memoryOpen = true;
									}}
								>
									Edit
								</Button>
								<Button
									variant="outline"
									size="sm"
									disabled={busy.has(m.id)}
									onclick={() => askDelete(m.id, 'Delete this memory?', `/memories/${m.id}`)}
								>
									Delete
								</Button>
							</Table.Cell>
						</Table.Row>
					{/each}
				</Table.Body>
			</Table.Root>
		{/if}
	</Tabs.Content>
</Tabs.Root>

<RuleDialog bind:open={ruleOpen} rule={editRule} {repositoryId} onsaved={load} />
<MemoryDialog bind:open={memoryOpen} memory={editMemory} {repositoryId} onsaved={load} />

<AlertDialog.Root bind:open={confirmOpen}>
	<AlertDialog.Content>
		<AlertDialog.Header>
			<AlertDialog.Title>{confirm?.title}</AlertDialog.Title>
			<AlertDialog.Description>{confirm?.text}</AlertDialog.Description>
		</AlertDialog.Header>
		<AlertDialog.Footer>
			<AlertDialog.Cancel>Cancel</AlertDialog.Cancel>
			<AlertDialog.Action onclick={() => confirm?.run()}>Delete</AlertDialog.Action>
		</AlertDialog.Footer>
	</AlertDialog.Content>
</AlertDialog.Root>
