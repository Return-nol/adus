<script lang="ts">
	import { onMount } from 'svelte';
	import { SvelteSet } from 'svelte/reactivity';
	import { toast } from 'svelte-sonner';
	import { api, ApiError } from '#lib/api.js';
	import { session } from '#lib/session.svelte.js';
	import AiKeyDialog from '#lib/components/ai-key-dialog.svelte';
	import ProviderDialog, {
		kindLabel,
		type AiKey,
		type Provider
	} from '#lib/components/provider-dialog.svelte';
	import * as AlertDialog from '#lib/components/ui/alert-dialog/index.js';
	import { Badge } from '#lib/components/ui/badge/index.js';
	import { Button } from '#lib/components/ui/button/index.js';
	import * as Card from '#lib/components/ui/card/index.js';
	import * as Dialog from '#lib/components/ui/dialog/index.js';
	import { Input } from '#lib/components/ui/input/index.js';
	import { Label } from '#lib/components/ui/label/index.js';
	import { Switch } from '#lib/components/ui/switch/index.js';
	import * as Table from '#lib/components/ui/table/index.js';

	type Settings = { default_provider_id: string | null; default_model: string | null };

	let providers = $state<Provider[]>([]);
	let settings = $state<Settings | null>(null);
	let loaded = $state(false);
	// Ids (provider, key) with a request in flight; their buttons are disabled.
	const busy = new SvelteSet<string>();

	let providerOpen = $state(false);
	let editProvider = $state<Provider | null>(null);
	let keyOpen = $state(false);
	let keyProviderId = $state('');
	let editKey = $state<AiKey | null>(null);
	let confirmOpen = $state(false);
	let confirm = $state<{ title: string; text: string; run: () => Promise<void> } | null>(null);
	let testOpen = $state(false);
	let testKey = $state<AiKey | null>(null);
	let testModel = $state('');

	// The server enforces admin too; this only hides/disables controls for role `user`.
	const isAdmin = $derived(session.user?.role === 'admin');
	const statusVariant = {
		ok: 'default',
		invalid: 'destructive',
		rate_limited: 'secondary'
	} as const;
	const statusLabel = { ok: 'ok', invalid: 'invalid', rate_limited: 'rate limited' } as const;

	function fail(e: unknown) {
		toast.error(e instanceof ApiError ? e.message : 'Could not reach the server');
	}

	async function load() {
		try {
			[providers, settings] = await Promise.all([
				api<Provider[]>('GET', '/ai-providers'),
				api<Settings>('GET', '/settings')
			]);
		} catch (e) {
			fail(e);
		}
		loaded = true;
	}

	onMount(load);

	/** Runs `fn` with `id` marked busy, then reloads (which also snaps a failed switch back). */
	async function guarded(id: string, fn: () => Promise<unknown>) {
		if (busy.has(id)) return;
		busy.add(id);
		try {
			await fn();
		} catch (e) {
			fail(e); // e.g. 409 provider_in_use
		}
		await load();
		busy.delete(id);
	}

	function askDelete(id: string, title: string, text: string, path: string) {
		confirm = { title, text, run: () => guarded(id, () => api('DELETE', path)) };
		confirmOpen = true;
	}

	function openTest(p: Provider, k: AiKey) {
		testKey = k;
		testModel = settings?.default_provider_id === p.id ? (settings.default_model ?? '') : '';
		testOpen = true;
	}

	async function runTest(event: SubmitEvent) {
		event.preventDefault();
		const k = testKey;
		if (!k || busy.has(k.id)) return;
		busy.add(k.id);
		try {
			const r = await api<{ ok: boolean; error?: string }>('POST', `/ai-keys/${k.id}/test`, {
				model: testModel
			});
			testOpen = false;
			if (r.ok) toast.success(`Key ${k.label} works`);
			else toast.error(`Key ${k.label} failed`, { description: r.error });
		} catch (e) {
			fail(e);
		}
		busy.delete(k.id);
	}

	function cooldown(k: AiKey) {
		if (!k.cooldown_until || new Date(k.cooldown_until) <= new Date()) return '—';
		return `until ${new Date(k.cooldown_until).toLocaleString()}`;
	}
</script>

<div class="mb-6 flex items-center justify-between">
	<h1 class="font-display text-3xl font-medium tracking-[0.01em] text-foreground">AI Providers</h1>
	{#if isAdmin}
		<Button
			onclick={() => {
				editProvider = null;
				providerOpen = true;
			}}
		>
			Add provider
		</Button>
	{/if}
</div>

{#if loaded && providers.length === 0}
	<div
		class="rounded-xl border border-dashed border-border bg-card px-8 py-12 text-center text-sm text-muted-foreground"
	>
		{isAdmin
			? 'Add an AI provider, then add one or more API keys to it.'
			: 'No AI providers yet. An admin needs to add one.'}
	</div>
{/if}

<div class="space-y-6">
	{#each providers as p (p.id)}
		<Card.Root>
			<Card.Header>
				<Card.Title class="flex flex-wrap items-center gap-2">
					{p.name}
					<Badge variant="secondary">{kindLabel[p.kind]}</Badge>
					{#if settings?.default_provider_id === p.id}<Badge>default</Badge>{/if}
					{#if !p.enabled}<Badge variant="outline">disabled</Badge>{/if}
				</Card.Title>
				{#if p.base_url}<Card.Description><code>{p.base_url}</code></Card.Description>{/if}
				{#if isAdmin}
					<Card.Action class="flex flex-wrap items-center gap-2">
						<Switch
							aria-label={`Enabled: ${p.name}`}
							disabled={busy.has(p.id)}
							bind:checked={
								() => p.enabled,
								(v) => guarded(p.id, () => api('PATCH', `/ai-providers/${p.id}`, { enabled: v }))
							}
						/>
						<Button
							variant="outline"
							size="sm"
							disabled={busy.has(p.id)}
							onclick={() => {
								editProvider = p;
								providerOpen = true;
							}}
						>
							Edit
						</Button>
						<Button
							variant="destructive"
							size="sm"
							disabled={busy.has(p.id)}
							onclick={() =>
								askDelete(
									p.id,
									`Delete provider ${p.name}?`,
									'Its keys are deleted too. A provider that is the default or used by a repository cannot be deleted.',
									`/ai-providers/${p.id}`
								)}
						>
							Delete
						</Button>
					</Card.Action>
				{/if}
			</Card.Header>
			<Card.Content class="space-y-4">
				<Table.Root>
					<Table.Header>
						<Table.Row>
							<Table.Head>Label</Table.Head>
							<Table.Head>Key</Table.Head>
							<Table.Head>Status</Table.Head>
							<Table.Head>Cooldown</Table.Head>
							<Table.Head>Enabled</Table.Head>
							<Table.Head class="text-right">Actions</Table.Head>
						</Table.Row>
					</Table.Header>
					<Table.Body>
						{#each p.keys as k (k.id)}
							<Table.Row>
								<Table.Cell>{k.label}</Table.Cell>
								<Table.Cell><code>••••{k.key_last4}</code></Table.Cell>
								<Table.Cell>
									<Badge variant={statusVariant[k.status]}>{statusLabel[k.status]}</Badge>
								</Table.Cell>
								<Table.Cell class="text-muted-foreground">{cooldown(k)}</Table.Cell>
								<Table.Cell>
									<Switch
										aria-label={`Enabled: ${k.label}`}
										disabled={!isAdmin || busy.has(k.id)}
										bind:checked={
											() => k.enabled,
											(v) => guarded(k.id, () => api('PATCH', `/ai-keys/${k.id}`, { enabled: v }))
										}
									/>
								</Table.Cell>
								<Table.Cell class="space-x-2 text-right">
									{#if isAdmin}
										<Button
											variant="outline"
											size="sm"
											disabled={busy.has(k.id)}
											onclick={() => openTest(p, k)}
										>
											Test
										</Button>
										<Button
											variant="outline"
											size="sm"
											disabled={busy.has(k.id)}
											onclick={() => {
												keyProviderId = p.id;
												editKey = k;
												keyOpen = true;
											}}
										>
											Edit
										</Button>
										<Button
											variant="outline"
											size="sm"
											disabled={busy.has(k.id)}
											onclick={() =>
												askDelete(
													k.id,
													`Delete key ${k.label}?`,
													'Usage history is kept. This cannot be undone.',
													`/ai-keys/${k.id}`
												)}
										>
											Delete
										</Button>
									{/if}
								</Table.Cell>
							</Table.Row>
						{:else}
							<Table.Row>
								<Table.Cell colspan={6} class="text-muted-foreground">No keys yet.</Table.Cell>
							</Table.Row>
						{/each}
					</Table.Body>
				</Table.Root>
				{#if isAdmin}
					<Button
						variant="outline"
						size="sm"
						onclick={() => {
							keyProviderId = p.id;
							editKey = null;
							keyOpen = true;
						}}
					>
						Add key
					</Button>
				{/if}
			</Card.Content>
		</Card.Root>
	{/each}
</div>

<ProviderDialog bind:open={providerOpen} provider={editProvider} onsaved={load} />
<AiKeyDialog bind:open={keyOpen} providerId={keyProviderId} aiKey={editKey} onsaved={load} />

<Dialog.Root bind:open={testOpen}>
	<Dialog.Content>
		<Dialog.Header>
			<Dialog.Title>Test key {testKey?.label}</Dialog.Title>
			<Dialog.Description>
				Sends one tiny request with this key. It is recorded in token usage.
			</Dialog.Description>
		</Dialog.Header>
		<form class="grid gap-4" onsubmit={runTest}>
			<div class="grid gap-2">
				<Label for="t-model">Model</Label>
				<Input id="t-model" required placeholder="e.g. gpt-4o-mini" bind:value={testModel} />
			</div>
			<Dialog.Footer>
				<Button type="submit" disabled={!testKey || busy.has(testKey.id)}>Run test</Button>
			</Dialog.Footer>
		</form>
	</Dialog.Content>
</Dialog.Root>

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
