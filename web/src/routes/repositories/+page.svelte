<script lang="ts">
	import { onMount } from 'svelte';
	import { toast } from 'svelte-sonner';
	import { api, ApiError } from '#lib/api.js';
	import { session } from '#lib/session.svelte.js';
	import AddRepoDialog from '#lib/components/add-repo-dialog.svelte';
	import ConnectionDialog, {
		type Connection,
		type WebhookError
	} from '#lib/components/connection-dialog.svelte';
	import * as AlertDialog from '#lib/components/ui/alert-dialog/index.js';
	import { Badge } from '#lib/components/ui/badge/index.js';
	import { Button } from '#lib/components/ui/button/index.js';
	import * as Card from '#lib/components/ui/card/index.js';
	import * as Select from '#lib/components/ui/select/index.js';
	import { Switch } from '#lib/components/ui/switch/index.js';
	import * as Table from '#lib/components/ui/table/index.js';
	import { Skeleton } from '#lib/components/ui/skeleton/index.js';
	import TableSkeleton from '#lib/components/table-skeleton.svelte';
	import type { Provider } from '#lib/components/provider-dialog.svelte';

	type Repo = {
		id: string;
		connection_id: string;
		display_name: string;
		name: string;
		enabled: boolean;
		auto_review: boolean;
		review_provider_id: string | null;
		webhook_count: number;
	};

	let connections = $state<Connection[]>([]);
	let repos = $state<Repo[]>([]);
	let providers = $state<Provider[]>([]);
	let loaded = $state(false);
	let connDialogOpen = $state(false);
	let addOpen = $state(false);
	let selected = $state<Connection | null>(null);
	let confirmOpen = $state(false);
	let confirm = $state<{ title: string; text: string; run: () => Promise<void> } | null>(null);

	// The server enforces admin too; this only disables controls for role `user`.
	const isAdmin = $derived(session.user?.role === 'admin');
	const kindLabel = { azure_devops: 'Azure DevOps', github: 'GitHub' };
	const statusVariant = { ok: 'default', invalid: 'destructive', unverified: 'secondary' } as const;

	function fail(e: unknown) {
		toast.error(e instanceof ApiError ? e.message : 'Could not reach the server');
	}

	async function load() {
		try {
			[connections, repos, providers] = await Promise.all([
				api<Connection[]>('GET', '/connections'),
				api<Repo[]>('GET', '/repositories'),
				api<Provider[]>('GET', '/ai-providers')
			]);
		} catch (e) {
			fail(e);
		}
		loaded = true;
	}

	function providerLabel(id: string | null): string {
		return (id && providers.find((p) => p.id === id)?.name) || 'Default (global)';
	}

	onMount(load);

	function warnWebhooks(errors: WebhookError[]) {
		if (errors.length === 0) return;
		const lines = errors.map((w) => {
			const repo = repos.find((r) => r.id === w.repository_id);
			return `${repo ? repo.display_name : w.repository_id}: ${w.message}`;
		});
		toast.warning('Some webhooks could not be synced', { description: lines.join('\n') });
	}

	async function afterSave(errors: WebhookError[]) {
		await load();
		warnWebhooks(errors);
	}

	async function resync(c: Connection) {
		try {
			const r = await api<{ synced: number; webhook_errors: WebhookError[] }>(
				'POST',
				`/connections/${c.id}/resync-webhooks`
			);
			toast.success(`Webhooks synced for ${r.synced} repositor${r.synced === 1 ? 'y' : 'ies'}`);
			await load();
			warnWebhooks(r.webhook_errors);
		} catch (e) {
			fail(e);
		}
	}

	async function patchRepo(
		repo: Repo,
		patch: { enabled?: boolean; auto_review?: boolean; review_provider_id?: string | null }
	) {
		try {
			await api('PATCH', `/repositories/${repo.id}`, patch);
		} catch (e) {
			fail(e);
		}
		await load(); // also snaps a failed switch back
	}

	function askDelete(title: string, text: string, path: string) {
		confirm = {
			title,
			text,
			run: async () => {
				try {
					await api('DELETE', path);
					await load();
				} catch (e) {
					fail(e); // e.g. 409 connection_in_use
				}
			}
		};
		confirmOpen = true;
	}
</script>

<div class="mb-6 flex items-center justify-between">
	<h1 class="font-display text-3xl font-medium tracking-[0.01em] text-foreground">Repositories</h1>
	{#if isAdmin}
		<Button
			disabled={!loaded}
			onclick={() => {
				selected = null;
				connDialogOpen = true;
			}}
		>
			Add connection
		</Button>
	{/if}
</div>

{#if !loaded}
	<div class="space-y-6">
		<Card.Root>
			<Card.Header>
				<div class="flex items-center gap-2">
					<Skeleton class="h-6 w-32" />
					<Skeleton class="h-5 w-24 rounded-full" />
					<Skeleton class="h-5 w-10 rounded-full" />
				</div>
				<Skeleton class="h-4 w-72" />
				{#if isAdmin}
					<Card.Action class="flex flex-wrap gap-2">
						{#each { length: 4 } as _, j (j)}<Skeleton class="h-8 w-28 rounded-lg" />{/each}
					</Card.Action>
				{/if}
			</Card.Header>
			<Card.Content>
				<TableSkeleton headers={['Repository', 'Enabled', 'Auto review', 'Provider', 'Actions']} />
			</Card.Content>
		</Card.Root>
	</div>
{:else if connections.length === 0}
	<div
		class="rounded-xl border border-dashed border-border bg-card px-8 py-12 text-center text-sm text-muted-foreground"
	>
		{isAdmin
			? 'Connect a Git provider to start reviewing pull requests.'
			: 'No connections yet. An admin needs to connect a Git provider.'}
	</div>
{/if}

<div class="space-y-6">
	{#each connections as c (c.id)}
		{@const connRepos = repos.filter((r) => r.connection_id === c.id)}
		<Card.Root>
			<Card.Header>
				<Card.Title class="flex flex-wrap items-center gap-2">
					{c.name}
					<Badge variant="secondary">{kindLabel[c.kind]}</Badge>
					<Badge variant={statusVariant[c.status]}>{c.status}</Badge>
				</Card.Title>
				<Card.Description>
					{c.org_or_owner} · PAT owner {c.pat_owner ?? 'unknown'} · <code>••••{c.token_last4}</code>
				</Card.Description>
				{#if isAdmin}
					<Card.Action class="flex flex-wrap gap-2">
						<Button
							variant="outline"
							size="sm"
							onclick={() => {
								selected = c;
								connDialogOpen = true;
							}}
						>
							Rotate PAT
						</Button>
						<Button variant="outline" size="sm" onclick={() => resync(c)}>Resync webhooks</Button>
						<Button
							variant="outline"
							size="sm"
							onclick={() => {
								selected = c;
								addOpen = true;
							}}
						>
							Add repositories
						</Button>
						<Button
							variant="destructive"
							size="sm"
							onclick={() =>
								askDelete(
									`Delete connection ${c.name}?`,
									'Remove its repositories first. This cannot be undone.',
									`/connections/${c.id}`
								)}
						>
							Delete
						</Button>
					</Card.Action>
				{/if}
			</Card.Header>
			<Card.Content>
				<Table.Root>
					<Table.Header>
						<Table.Row>
							<Table.Head>Repository</Table.Head>
							<Table.Head>Enabled</Table.Head>
							<Table.Head>Auto review</Table.Head>
							<Table.Head>Provider</Table.Head>
							<Table.Head class="text-right">Actions</Table.Head>
						</Table.Row>
					</Table.Header>
					<Table.Body>
						{#each connRepos as repo (repo.id)}
							<Table.Row>
								<Table.Cell>{repo.display_name}</Table.Cell>
								<Table.Cell>
									<Switch
										aria-label={`Enabled: ${repo.display_name}`}
										disabled={!isAdmin}
										bind:checked={() => repo.enabled, (v) => patchRepo(repo, { enabled: v })}
									/>
								</Table.Cell>
								<Table.Cell>
									<Switch
										aria-label={`Auto review: ${repo.display_name}`}
										disabled={!isAdmin}
										bind:checked={
											() => repo.auto_review, (v) => patchRepo(repo, { auto_review: v })
										}
									/>
								</Table.Cell>
								<Table.Cell>
									{#if isAdmin}
										<Select.Root
											type="single"
											bind:value={
												() => repo.review_provider_id ?? '',
												(v) => patchRepo(repo, { review_provider_id: v || null })
											}
										>
											<Select.Trigger
												size="sm"
												class="w-48"
												aria-label={`Provider: ${repo.display_name}`}
											>
												<span class="truncate">{providerLabel(repo.review_provider_id)}</span>
											</Select.Trigger>
											<Select.Content>
												<Select.Item value="" label="Default (global)">
													Default (global)
												</Select.Item>
												{#each providers as p (p.id)}
													<Select.Item value={p.id} label={p.name}>{p.name}</Select.Item>
												{/each}
											</Select.Content>
										</Select.Root>
									{:else}
										<span class="text-muted-foreground">
											{providerLabel(repo.review_provider_id)}
										</span>
									{/if}
								</Table.Cell>
								<Table.Cell class="text-right">
									{#if isAdmin}
										<Button
											variant="outline"
											size="sm"
											onclick={() =>
												askDelete(
													`Remove ${repo.display_name}?`,
													'Its webhooks are deleted from the provider.',
													`/repositories/${repo.id}`
												)}
										>
											Remove
										</Button>
									{/if}
								</Table.Cell>
							</Table.Row>
						{:else}
							<Table.Row>
								<Table.Cell colspan={5} class="text-muted-foreground">
									No repositories yet.
								</Table.Cell>
							</Table.Row>
						{/each}
					</Table.Body>
				</Table.Root>
			</Card.Content>
		</Card.Root>
	{/each}
</div>

<ConnectionDialog bind:open={connDialogOpen} connection={selected} onsaved={afterSave} />
<AddRepoDialog bind:open={addOpen} connection={selected} onadded={load} />

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
				Delete
			</AlertDialog.Action>
		</AlertDialog.Footer>
	</AlertDialog.Content>
</AlertDialog.Root>
