<script lang="ts">
	import { onMount } from 'svelte';
	import { SvelteSet } from 'svelte/reactivity';
	import { toast } from 'svelte-sonner';
	import { api, ApiError } from '#lib/api.js';
	import { session } from '#lib/session.svelte.js';
	import MemoryDialog, {
		kindLabel,
		type Memory,
		type MemoryKind
	} from '#lib/components/memory-dialog.svelte';
	import RuleDialog, {
		severityLabel,
		type Rule,
		type Severity
	} from '#lib/components/rule-dialog.svelte';
	import * as AlertDialog from '#lib/components/ui/alert-dialog/index.js';
	import { Badge } from '#lib/components/ui/badge/index.js';
	import { Button } from '#lib/components/ui/button/index.js';
	import * as Card from '#lib/components/ui/card/index.js';
	import * as Select from '#lib/components/ui/select/index.js';
	import { Switch } from '#lib/components/ui/switch/index.js';
	import * as Table from '#lib/components/ui/table/index.js';
	import TableSkeleton from '#lib/components/table-skeleton.svelte';
	import * as Tabs from '#lib/components/ui/tabs/index.js';
	import * as Tooltip from '#lib/components/ui/tooltip/index.js';
	import { safeHttpUrl } from '#lib/utils.js';

	type Repo = { id: string; display_name: string };

	const GLOBAL = 'global';
	const ALL_KINDS = 'all';
	const MEMORY_PREVIEW = 120;
	const sourceLabel: Record<Memory['source'], string> = {
		manual: 'manual',
		pr_comment: 'PR comment',
		pr_learning: 'Learned from PR'
	};
	const severityVariant: Record<Severity, 'secondary' | 'default' | 'destructive'> = {
		info: 'secondary',
		warning: 'default',
		critical: 'destructive'
	};

	let repos = $state<Repo[]>([]);
	let scope = $state(GLOBAL);
	let kindFilter = $state<string>(ALL_KINDS);
	let rules = $state<Rule[]>([]);
	let memories = $state<Memory[]>([]);
	let loaded = $state(false);
	let loadError = $state(false);
	// Learned memories awaiting a decision, across all repositories (not tied to `scope`).
	let pending = $state<Memory[]>([]);
	let rejected = $state<Memory[]>([]);
	let activeById = $state<Record<string, Memory>>({});
	let learnedLoaded = $state(false);
	let learnedError = $state(false);
	// Ids (rule, memory) with a request in flight; their controls are disabled.
	const busy = new SvelteSet<string>();
	const expanded = new SvelteSet<string>();

	let ruleOpen = $state(false);
	let editRule = $state<Rule | null>(null);
	let memoryOpen = $state(false);
	let editMemory = $state<Memory | null>(null);
	let approveMemory = $state(false);
	let confirmOpen = $state(false);
	let confirm = $state<{
		title: string;
		text: string;
		action: string;
		run: () => Promise<void>;
	} | null>(null);

	// The server enforces admin too; this only hides pending/rejected controls for role `user`.
	const isAdmin = $derived(session.user?.role === 'admin');
	const learnedHeaders = $derived(
		isAdmin
			? ['Content', 'Kind', 'Repository', 'Source', 'Actions']
			: ['Content', 'Kind', 'Repository', 'Source']
	);

	const repositoryId = $derived(scope === GLOBAL ? null : scope);
	const scopeLabel = $derived.by(() => {
		const r = repos.find((r) => r.id === scope);
		return r ? r.display_name : 'Global';
	});

	function fail(e: unknown) {
		toast.error(e instanceof ApiError ? e.message : 'Could not reach the server');
	}

	function toggleExpanded(id: string) {
		if (expanded.has(id)) expanded.delete(id);
		else expanded.add(id);
	}

	function hasCuratedDetails(m: Memory): boolean {
		return Boolean(
			m.context || m.reason || m.resolution || m.evidence.some((ev) => safeHttpUrl(ev.url))
		);
	}

	// Only the newest request may write results, so a slow response for a previous scope is dropped.
	let seq = 0;
	async function load() {
		const mine = ++seq;
		loadError = false;
		const q = repositoryId ? `?repository_id=${repositoryId}` : '';
		const params = new URLSearchParams({ status: 'active' });
		if (repositoryId) params.set('repository_id', repositoryId);
		if (kindFilter !== ALL_KINDS) params.set('kind', kindFilter);
		const mq = `?${params.toString()}`;
		try {
			const [r, m] = await Promise.all([
				api<Rule[]>('GET', `/rules${q}`),
				api<Memory[]>('GET', `/memories${mq}`)
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

	async function loadLearned() {
		learnedError = false;
		try {
			const [p, r, activeAll] = await Promise.all([
				api<Memory[]>('GET', '/memories?all=true&status=pending'),
				api<Memory[]>('GET', '/memories?all=true&status=rejected'),
				api<Memory[]>('GET', '/memories?all=true&status=active')
			]);
			pending = p;
			rejected = r;
			activeById = Object.fromEntries(activeAll.map((m) => [m.id, m]));
		} catch (e) {
			learnedError = true;
			fail(e);
		}
		learnedLoaded = true;
	}

	function reloadAll() {
		return Promise.all([load(), loadLearned()]);
	}

	function repoName(id: string | null): string {
		return repos.find((r) => r.id === id)?.display_name ?? '—';
	}

	onMount(async () => {
		loadLearned();
		try {
			repos = await api<Repo[]>('GET', '/repositories');
		} catch (e) {
			fail(e);
		}
	});

	// Reload whenever the scope or kind filter changes (also runs once on mount).
	$effect(() => {
		void scope;
		void kindFilter;
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
		await reloadAll();
		busy.delete(id);
	}

	function askDelete(id: string, title: string, path: string) {
		confirm = {
			title,
			text: 'This cannot be undone.',
			action: 'Delete',
			run: () => guarded(id, () => api('DELETE', path))
		};
		confirmOpen = true;
	}

	function approve(m: Memory) {
		guarded(m.id, async () => {
			await api('POST', `/memories/${m.id}/approve`, {});
			toast.success('Memory approved');
		});
	}

	function openMemory(m: Memory | null, approving: boolean) {
		editMemory = m;
		approveMemory = approving;
		memoryOpen = true;
	}

	function askReject(m: Memory) {
		confirm = {
			title: 'Reject this memory?',
			text: 'It will not be used in reviews. Deleting it later from Rejected lets it be proposed again.',
			action: 'Reject',
			run: () =>
				guarded(m.id, async () => {
					await api('POST', `/memories/${m.id}/reject`, {});
					toast.success('Memory rejected');
				})
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

{#snippet loadFailed(text: string, retry: () => void)}
	<div
		class="rounded-xl border border-dashed border-border bg-card px-8 py-12 text-center text-sm text-muted-foreground"
	>
		{text}
		<Button variant="outline" size="sm" class="ml-2" onclick={retry}>Retry</Button>
	</div>
{/snippet}

{#snippet emptyState(text: string)}
	<div
		class="rounded-xl border border-dashed border-border bg-card px-8 py-12 text-center text-sm text-muted-foreground"
	>
		{text}
	</div>
{/snippet}

{#snippet memoryContent(m: Memory)}
	{#if m.status === 'pending' && m.supersedes}
		{@const target = activeById[m.supersedes]}
		<div class="mb-2 rounded-md border border-border bg-muted/40 p-2 text-xs">
			<div class="font-medium text-muted-foreground">Replaces active memory:</div>
			<div class="mt-0.5 text-muted-foreground line-through">
				{target ? target.content : m.supersedes}
			</div>
		</div>
	{/if}
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
	{#if hasCuratedDetails(m)}
		<button
			type="button"
			class="mt-1 block text-xs text-primary underline-offset-4 hover:underline"
			aria-expanded={expanded.has(m.id)}
			onclick={() => toggleExpanded(m.id)}
		>
			{expanded.has(m.id) ? 'Hide details' : 'Show details'}
		</button>
		{#if expanded.has(m.id)}
			<div class="mt-2 space-y-1.5 rounded-md border border-border bg-muted/40 p-2.5 text-xs">
				{#if m.context}
					<div>
						<span class="font-medium text-foreground">Context:</span>
						<span class="whitespace-pre-wrap text-muted-foreground">{m.context}</span>
					</div>
				{/if}
				{#if m.reason}
					<div>
						<span class="font-medium text-foreground">Reason:</span>
						<span class="whitespace-pre-wrap text-muted-foreground">{m.reason}</span>
					</div>
				{/if}
				{#if m.resolution}
					<div>
						<span class="font-medium text-foreground">Resolution:</span>
						<span class="whitespace-pre-wrap text-muted-foreground">{m.resolution}</span>
					</div>
				{/if}
				{#if m.evidence.some((ev) => safeHttpUrl(ev.url))}
					<div class="flex flex-wrap items-center gap-2">
						<span class="font-medium text-foreground">Evidence:</span>
						{#each m.evidence as ev, i (ev.url + i)}
							{@const href = safeHttpUrl(ev.url)}
							{#if href}
								<a
									{href}
									target="_blank"
									rel="noopener noreferrer"
									class="text-primary underline-offset-4 hover:underline"
								>
									{ev.pr_number != null ? `PR #${ev.pr_number}` : 'Link'}
								</a>
							{/if}
						{/each}
					</div>
				{/if}
			</div>
		{/if}
	{/if}
{/snippet}

<!-- Pending or rejected learned memories; only admins get the actions column. -->
{#snippet learnedTable(items: Memory[])}
	<Card.Root class="py-0">
		<Card.Content class="px-0">
			<Table.Root>
				<Table.Header>
					<Table.Row>
						<Table.Head>Content</Table.Head>
						<Table.Head>Kind</Table.Head>
						<Table.Head>Repository</Table.Head>
						<Table.Head>Source</Table.Head>
						{#if isAdmin}<Table.Head class="text-right">Actions</Table.Head>{/if}
					</Table.Row>
				</Table.Header>
				<Table.Body>
					{#each items as m (m.id)}
						<Table.Row>
							<Table.Cell class="max-w-md whitespace-normal">{@render memoryContent(m)}</Table.Cell>
							<Table.Cell><Badge variant="outline">{kindLabel[m.kind]}</Badge></Table.Cell>
							<Table.Cell>{repoName(m.repository_id)}</Table.Cell>
							<Table.Cell>
								{#if safeHttpUrl(m.source_url)}
									<a
										href={safeHttpUrl(m.source_url)}
										target="_blank"
										rel="noopener noreferrer"
										class="text-sm text-primary underline-offset-4 hover:underline"
									>
										View thread
									</a>
								{:else}
									<span class="text-muted-foreground">—</span>
								{/if}
							</Table.Cell>
							{#if isAdmin}
								<Table.Cell class="space-x-2 text-right">
									{#if m.status === 'pending'}
										<Button size="sm" disabled={busy.has(m.id)} onclick={() => approve(m)}>
											Approve
										</Button>
										<Button
											variant="outline"
											size="sm"
											disabled={busy.has(m.id)}
											onclick={() => openMemory(m, true)}
										>
											Edit
										</Button>
										<Button
											variant="outline"
											size="sm"
											disabled={busy.has(m.id)}
											onclick={() => askReject(m)}
										>
											Reject
										</Button>
									{:else}
										<Button
											variant="outline"
											size="sm"
											disabled={busy.has(m.id)}
											onclick={() => askDelete(m.id, 'Delete this memory?', `/memories/${m.id}`)}
										>
											Delete
										</Button>
									{/if}
								</Table.Cell>
							{/if}
						</Table.Row>
					{/each}
				</Table.Body>
			</Table.Root>
		</Card.Content>
	</Card.Root>
{/snippet}

<Tabs.Root value="rules">
	<Tabs.List>
		<Tabs.Trigger value="rules">Rules</Tabs.Trigger>
		<Tabs.Trigger value="memory">Memory</Tabs.Trigger>
		<Tabs.Trigger value="pending">
			Pending
			{#if pending.length > 0}<Badge variant="secondary">{pending.length}</Badge>{/if}
		</Tabs.Trigger>
		{#if isAdmin}<Tabs.Trigger value="rejected">Rejected</Tabs.Trigger>{/if}
	</Tabs.List>

	<Tabs.Content value="rules" class="space-y-4">
		<div class="flex justify-end">
			<Button
				disabled={!loaded}
				onclick={() => {
					editRule = null;
					ruleOpen = true;
				}}
			>
				Add rule
			</Button>
		</div>
		{#if loadError}
			{@render loadFailed('Could not load this scope.', load)}
		{:else if !loaded}
			<TableSkeleton card headers={['Title', 'Path glob', 'Severity', 'Enabled', 'Actions']} />
		{:else if rules.length === 0}
			<div
				class="rounded-xl border border-dashed border-border bg-card px-8 py-12 text-center text-sm text-muted-foreground"
			>
				No rules for this scope yet. Rules tell the reviewer what to look for.
			</div>
		{:else if rules.length > 0}
			<Card.Root class="py-0">
				<Card.Content class="px-0">
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
				</Card.Content>
			</Card.Root>
		{/if}
	</Tabs.Content>

	<Tabs.Content value="memory" class="space-y-4">
		<div class="flex flex-wrap items-center justify-between gap-3">
			<Select.Root type="single" bind:value={kindFilter}>
				<Select.Trigger class="w-48" aria-label="Kind filter">
					{kindFilter === ALL_KINDS ? 'All kinds' : kindLabel[kindFilter as MemoryKind]}
				</Select.Trigger>
				<Select.Content>
					<Select.Item value={ALL_KINDS} label="All kinds">All kinds</Select.Item>
					{#each Object.entries(kindLabel) as [value, label] (value)}
						<Select.Item {value} {label}>{label}</Select.Item>
					{/each}
				</Select.Content>
			</Select.Root>
			<Button disabled={!loaded} onclick={() => openMemory(null, false)}>Add memory</Button>
		</div>
		{#if loadError}
			{@render loadFailed('Could not load this scope.', load)}
		{:else if !loaded}
			<TableSkeleton
				card
				headers={['Content', 'Kind', 'Source', 'Created by', 'Enabled', 'Actions']}
			/>
		{:else if memories.length === 0}
			<div
				class="rounded-xl border border-dashed border-border bg-card px-8 py-12 text-center text-sm text-muted-foreground"
			>
				No memories for this scope yet. Add one here, or comment <code>@adudu remember: …</code> on a
				pull request.
			</div>
		{:else if memories.length > 0}
			<Card.Root class="py-0">
				<Card.Content class="px-0">
					<Table.Root>
						<Table.Header>
							<Table.Row>
								<Table.Head>Content</Table.Head>
								<Table.Head>Kind</Table.Head>
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
										{@render memoryContent(m)}
									</Table.Cell>
									<Table.Cell>
										<Badge variant="outline">{kindLabel[m.kind]}</Badge>
									</Table.Cell>
									<Table.Cell>
										<Badge variant="secondary">
											{sourceLabel[m.source]}
										</Badge>
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
									<Table.Cell class="text-muted-foreground">
										{m.created_by?.email ?? '—'}
									</Table.Cell>
									<Table.Cell>
										<Switch
											aria-label="Enabled"
											disabled={busy.has(m.id)}
											bind:checked={
												() => m.enabled,
												(v) =>
													guarded(m.id, () => api('PATCH', `/memories/${m.id}`, { enabled: v }))
											}
										/>
									</Table.Cell>
									<Table.Cell class="space-x-2 text-right">
										<Button
											variant="outline"
											size="sm"
											disabled={busy.has(m.id)}
											onclick={() => openMemory(m, false)}
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
				</Card.Content>
			</Card.Root>
		{/if}
	</Tabs.Content>

	<Tabs.Content value="pending" class="space-y-4">
		{#if learnedError}
			{@render loadFailed('Could not load learned memories.', loadLearned)}
		{:else if !learnedLoaded}
			<TableSkeleton card headers={learnedHeaders} />
		{:else if pending.length === 0}
			{@render emptyState(
				'No learned memories are waiting for approval. Closed pull requests propose them when a repository learns with approval.'
			)}
		{:else}
			{@render learnedTable(pending)}
		{/if}
	</Tabs.Content>

	{#if isAdmin}
		<Tabs.Content value="rejected" class="space-y-4">
			{#if learnedError}
				{@render loadFailed('Could not load learned memories.', loadLearned)}
			{:else if !learnedLoaded}
				<TableSkeleton card headers={learnedHeaders} />
			{:else if rejected.length === 0}
				{@render emptyState('No rejected memories.')}
			{:else}
				{@render learnedTable(rejected)}
			{/if}
		</Tabs.Content>
	{/if}
</Tabs.Root>

<RuleDialog bind:open={ruleOpen} rule={editRule} {repositoryId} onsaved={load} />
<MemoryDialog
	bind:open={memoryOpen}
	memory={editMemory}
	{repositoryId}
	approve={approveMemory}
	onsaved={reloadAll}
/>

<AlertDialog.Root bind:open={confirmOpen}>
	<AlertDialog.Content>
		<AlertDialog.Header>
			<AlertDialog.Title>{confirm?.title}</AlertDialog.Title>
			<AlertDialog.Description>{confirm?.text}</AlertDialog.Description>
		</AlertDialog.Header>
		<AlertDialog.Footer>
			<AlertDialog.Cancel>Cancel</AlertDialog.Cancel>
			<AlertDialog.Action
				onclick={() => {
					confirmOpen = false;
					confirm?.run();
				}}
			>
				{confirm?.action}
			</AlertDialog.Action>
		</AlertDialog.Footer>
	</AlertDialog.Content>
</AlertDialog.Root>
