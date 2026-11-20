<script lang="ts">
	import { onMount } from 'svelte';
	import { toast } from 'svelte-sonner';
	import { api, ApiError } from '#lib/api.js';
	import { Badge, type BadgeVariant } from '#lib/components/ui/badge/index.js';
	import { Button } from '#lib/components/ui/button/index.js';
	import * as Card from '#lib/components/ui/card/index.js';
	import * as Select from '#lib/components/ui/select/index.js';
	import * as Table from '#lib/components/ui/table/index.js';
	import * as Tooltip from '#lib/components/ui/tooltip/index.js';
	import { safeHttpUrl } from '#lib/utils.js';

	type Status = 'queued' | 'running' | 'done' | 'failed' | 'skipped';
	type Trigger = 'opened' | 'updated' | 'command';
	type Review = {
		id: string;
		repository: { id: string; name: string; kind: 'github' | 'azure_devops' };
		pr_number: number;
		pr_url: string | null;
		trigger: Trigger;
		status: Status;
		error: string | null;
		skip_reason: string | null;
		head_sha: string | null;
		comments_posted: number;
		input_tokens: number;
		output_tokens: number;
		started_at: string | null;
		finished_at: string | null;
	};
	type Repo = { id: string; owner: string; name: string };

	const ALL = 'all';
	const PER_PAGE = 20;
	const REFRESH_MS = 15_000;
	const statuses: Status[] = ['queued', 'running', 'done', 'failed', 'skipped'];
	const statusVariant: Record<Status, BadgeVariant> = {
		queued: 'outline',
		running: 'default',
		done: 'secondary',
		failed: 'destructive',
		skipped: 'outline'
	};
	const triggerLabel: Record<Trigger, string> = {
		opened: 'PR opened',
		updated: 'PR updated',
		command: '@adudu review'
	};
	// Reasons written by the review engine; anything unknown falls back to the raw code.
	const skipLabel: Record<string, string> = {
		repository_disabled: 'repository disabled',
		auto_review_off: 'auto review off',
		draft: 'draft PR',
		already_reviewed: 'already reviewed',
		nothing_to_review: 'nothing to review'
	};
	const dateFormat = new Intl.DateTimeFormat(undefined, {
		dateStyle: 'medium',
		timeStyle: 'short'
	});

	let repos = $state<Repo[]>([]);
	let repoFilter = $state(ALL);
	let statusFilter = $state(ALL);
	let page = $state(1);
	let items = $state<Review[]>([]);
	let total = $state(0);
	let loaded = $state(false);
	let loadError = $state(false);

	const pages = $derived(Math.max(1, Math.ceil(total / PER_PAGE)));
	const filtered = $derived(repoFilter !== ALL || statusFilter !== ALL);
	const repoLabel = $derived.by(() => {
		const r = repos.find((r) => r.id === repoFilter);
		return r ? `${r.owner}/${r.name}` : 'All repositories';
	});
	const active = $derived(items.some((r) => r.status === 'queued' || r.status === 'running'));

	const n = (v: number) => v.toLocaleString();

	function duration(r: Review): string {
		if (!r.started_at || !r.finished_at) return '—';
		const secs = Math.max(
			0,
			Math.round((Date.parse(r.finished_at) - Date.parse(r.started_at)) / 1000)
		);
		return secs < 60 ? `${secs}s` : `${Math.floor(secs / 60)}m ${secs % 60}s`;
	}

	function fail(e: unknown) {
		toast.error(e instanceof ApiError ? e.message : 'Could not reach the server');
	}

	// Only the newest request may write results, so a slow response for a previous filter is dropped.
	// A failed background refresh keeps the current rows and toasts once until a refresh succeeds;
	// only a foreground failure swaps the table for the error card.
	let seq = 0;
	let refreshFailing = false;
	async function load(background = false) {
		const mine = ++seq;
		const params = new URLSearchParams({ page: String(page), per_page: String(PER_PAGE) });
		if (repoFilter !== ALL) params.set('repository_id', repoFilter);
		if (statusFilter !== ALL) params.set('status', statusFilter);
		try {
			const res = await api<{ items: Review[]; total: number }>('GET', `/reviews?${params}`);
			if (mine !== seq) return;
			items = res.items;
			total = res.total;
			loadError = false;
			refreshFailing = false;
			// The list shrank under us (e.g. filtered by `running`): jump to the last page.
			if (page > pages && total > 0) page = pages;
		} catch (e) {
			if (mine !== seq) return;
			if (!background) {
				loadError = true;
				fail(e);
			} else if (!refreshFailing) {
				refreshFailing = true;
				fail(e);
			}
		}
		loaded = true;
	}

	onMount(async () => {
		try {
			repos = await api<Repo[]>('GET', '/repositories');
		} catch (e) {
			fail(e);
		}
	});

	// Reload whenever the page or a filter changes (also runs once on mount).
	$effect(() => {
		void [page, repoFilter, statusFilter];
		loaded = false;
		items = [];
		load();
	});

	// Poll only while something is still queued or running; the cleanup also runs on page destroy.
	$effect(() => {
		if (!active) return;
		const timer = setInterval(() => load(true), REFRESH_MS);
		return () => clearInterval(timer);
	});
</script>

<div class="mb-6 flex flex-wrap items-center justify-between gap-3">
	<h1 class="font-display text-3xl font-medium tracking-[0.01em] text-foreground">Reviews</h1>
	<div class="flex flex-wrap gap-2">
		<Select.Root type="single" bind:value={repoFilter} onValueChange={() => (page = 1)}>
			<Select.Trigger class="w-64" aria-label="Repository">{repoLabel}</Select.Trigger>
			<Select.Content>
				<Select.Item value={ALL} label="All repositories">All repositories</Select.Item>
				{#each repos as r (r.id)}
					<Select.Item value={r.id} label={`${r.owner}/${r.name}`}>{r.owner}/{r.name}</Select.Item>
				{/each}
			</Select.Content>
		</Select.Root>
		<Select.Root type="single" bind:value={statusFilter} onValueChange={() => (page = 1)}>
			<Select.Trigger class="w-40 capitalize" aria-label="Status">
				{statusFilter === ALL ? 'All statuses' : statusFilter}
			</Select.Trigger>
			<Select.Content>
				<Select.Item value={ALL} label="All statuses">All statuses</Select.Item>
				{#each statuses as s (s)}
					<Select.Item value={s} label={s} class="capitalize">{s}</Select.Item>
				{/each}
			</Select.Content>
		</Select.Root>
	</div>
</div>

{#if loadError}
	<div
		class="rounded-xl border border-dashed border-border bg-card px-8 py-12 text-center text-sm text-muted-foreground"
	>
		Could not load reviews.
		<Button variant="outline" size="sm" class="ml-2" onclick={() => load()}>Retry</Button>
	</div>
{:else if loaded && items.length === 0}
	<div
		class="rounded-xl border border-dashed border-border bg-card px-8 py-12 text-center text-sm text-muted-foreground"
	>
		{#if filtered}
			No reviews match these filters.
		{:else}
			No reviews yet. A review runs when a pull request is opened or updated, or when someone
			comments <code>@adudu review</code>.
		{/if}
	</div>
{:else if items.length > 0}
<Card.Root class="py-0">
		<Card.Content class="px-0">
	<Table.Root>
		<Table.Header>
			<Table.Row>
				<Table.Head>Repository</Table.Head>
				<Table.Head>PR</Table.Head>
				<Table.Head>Trigger</Table.Head>
				<Table.Head>Status</Table.Head>
				<Table.Head class="text-right">Comments</Table.Head>
				<Table.Head class="text-right">Input tokens</Table.Head>
				<Table.Head class="text-right">Output tokens</Table.Head>
				<Table.Head>Started</Table.Head>
				<Table.Head class="text-right">Duration</Table.Head>
			</Table.Row>
		</Table.Header>
		<Table.Body>
			{#each items as r (r.id)}
				<Table.Row>
					<Table.Cell class="font-medium">{r.repository.name}</Table.Cell>
					<Table.Cell>
						{#if safeHttpUrl(r.pr_url)}
							<a
								href={safeHttpUrl(r.pr_url)}
								target="_blank"
								rel="noopener noreferrer"
								class="text-primary underline-offset-4 hover:underline"
							>
								#{r.pr_number}
							</a>
						{:else}
							#{r.pr_number}
						{/if}
					</Table.Cell>
					<Table.Cell class="text-muted-foreground">{triggerLabel[r.trigger]}</Table.Cell>
					<Table.Cell>
						{#if r.status === 'failed' && r.error}
							<Tooltip.Root>
								<Tooltip.Trigger title={r.error}>
									<Badge variant={statusVariant.failed}>failed</Badge>
								</Tooltip.Trigger>
								<Tooltip.Content class="max-w-sm whitespace-pre-wrap">{r.error}</Tooltip.Content>
							</Tooltip.Root>
						{:else}
							<Badge variant={statusVariant[r.status]}>{r.status}</Badge>
						{/if}
						{#if r.status === 'skipped' && r.skip_reason}
							<span class="ml-1 text-xs text-muted-foreground">
								{skipLabel[r.skip_reason] ?? r.skip_reason}
							</span>
						{/if}
					</Table.Cell>
					<Table.Cell class="text-right tabular-nums">{n(r.comments_posted)}</Table.Cell>
					<Table.Cell class="text-right tabular-nums">{n(r.input_tokens)}</Table.Cell>
					<Table.Cell class="text-right tabular-nums">{n(r.output_tokens)}</Table.Cell>
					<Table.Cell class="text-muted-foreground">
						{#if r.started_at}
							<time datetime={r.started_at}>{dateFormat.format(new Date(r.started_at))}</time>
						{:else}
							—
						{/if}
					</Table.Cell>
					<Table.Cell class="text-right text-muted-foreground tabular-nums">
						{duration(r)}
					</Table.Cell>
				</Table.Row>
			{/each}
		</Table.Body>
	</Table.Root>
		</Card.Content>
</Card.Root>

	<div class="mt-4 flex items-center justify-end gap-3 text-sm text-muted-foreground">
		<span>Page {page} of {pages}</span>
		<Button variant="outline" size="sm" disabled={page <= 1} onclick={() => (page -= 1)}>
			Prev
		</Button>
		<Button variant="outline" size="sm" disabled={page >= pages} onclick={() => (page += 1)}>
			Next
		</Button>
	</div>
{/if}
