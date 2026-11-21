<script lang="ts" module>
	export type ProviderKind = 'openai' | 'anthropic' | 'gemini' | 'openai_compatible';
	export type AiKey = {
		id: string;
		label: string;
		key_last4: string;
		enabled: boolean;
		status: 'ok' | 'invalid' | 'rate_limited';
		cooldown_until: string | null;
		last_used_at: string | null;
	};
	export type Provider = {
		id: string;
		kind: ProviderKind;
		name: string;
		base_url: string | null;
		/** null = set up before models moved onto providers; needs a model. */
		primary_model: string | null;
		fast_model: string | null;
		enabled: boolean;
		keys: AiKey[];
	};
	export const kindLabel: Record<ProviderKind, string> = {
		openai: 'OpenAI',
		anthropic: 'Anthropic',
		gemini: 'Gemini',
		openai_compatible: 'OpenAI-compatible'
	};
</script>

<script lang="ts">
	import { toast } from 'svelte-sonner';
	import RiArrowDownSLine from 'remixicon-svelte/icons/arrow-down-s-line';
	import RiLoader3Line from 'remixicon-svelte/icons/loader-3-line';
	import { api, ApiError } from '#lib/api.js';
	import { Button } from '#lib/components/ui/button/index.js';
	import * as Command from '#lib/components/ui/command/index.js';
	import * as Dialog from '#lib/components/ui/dialog/index.js';
	import { Input } from '#lib/components/ui/input/index.js';
	import { Label } from '#lib/components/ui/label/index.js';
	import * as Popover from '#lib/components/ui/popover/index.js';
	import * as Select from '#lib/components/ui/select/index.js';

	// `provider` set = edit it (kind is fixed); null = create a new provider.
	let {
		open = $bindable(false),
		provider,
		onsaved
	}: { open: boolean; provider: Provider | null; onsaved: () => void } = $props();

	let kind = $state<ProviderKind>('openai');
	let name = $state('');
	let baseUrl = $state('');
	let apiKey = $state('');
	let primaryModel = $state('');
	let fastModel = $state('');
	let models = $state<string[]>([]);
	let tested = $state(false);
	let testing = $state(false);
	let busy = $state(false);
	let primaryOpen = $state(false);
	let fastOpen = $state(false);
	let primarySearch = $state('');
	let fastSearch = $state('');

	// Reset the form each time the dialog opens, and drop any entered API key on close.
	$effect(() => {
		if (!open) {
			apiKey = '';
			return;
		}
		kind = provider?.kind ?? 'openai';
		name = provider?.name ?? '';
		baseUrl = provider?.base_url ?? '';
		apiKey = '';
		primaryModel = provider?.primary_model ?? '';
		fastModel = provider?.fast_model ?? '';
		models = [];
		tested = false;
		testing = false;
		primaryOpen = false;
		fastOpen = false;
		primarySearch = '';
		fastSearch = '';
	});

	const shownKind = $derived(provider?.kind ?? kind);
	// A saved key is only ever sent to the saved base URL: a new URL needs the key again.
	const urlChanged = $derived(
		Boolean(provider) &&
			baseUrl.trim().replace(/\/+$/, '') !== (provider?.base_url ?? '').trim().replace(/\/+$/, '')
	);
	const needsKey = $derived(!provider || provider.keys.length === 0 || urlChanged);
	const canTest = $derived(
		!testing &&
			!busy &&
			(!needsKey || Boolean(apiKey.trim())) &&
			(shownKind !== 'openai_compatible' || Boolean(baseUrl.trim()))
	);
	const canSave = $derived(
		!busy && !testing && tested && Boolean(name.trim()) && Boolean(primaryModel.trim())
	);
	const customPrimary = $derived(
		primarySearch.trim() && !models.includes(primarySearch.trim()) ? primarySearch.trim() : ''
	);
	const customFast = $derived(
		fastSearch.trim() && !models.includes(fastSearch.trim()) ? fastSearch.trim() : ''
	);

	function resetConnection() {
		tested = false;
		models = [];
	}

	async function testConnection() {
		if (!canTest) return;
		testing = true;
		try {
			const trimmedUrl = baseUrl.trim();
			const trimmedKey = apiKey.trim();
			const reply = await api<{ models: string[] }>('POST', '/ai-providers/models', {
				...(provider ? { provider_id: provider.id, base_url: trimmedUrl } : { kind }),
				...(!provider && trimmedUrl ? { base_url: trimmedUrl } : {}),
				...(trimmedKey ? { key: trimmedKey } : {}),
				// A connection test always asks the provider, never the server's model-list cache.
				refresh: true
			});
			models = reply.models;
			tested = true;
			toast.success(`Connected — ${models.length} model${models.length === 1 ? '' : 's'} loaded`);
		} catch (e) {
			resetConnection();
			toast.error(e instanceof ApiError ? e.message : 'Could not reach the server');
		} finally {
			testing = false;
		}
	}

	async function submit(event: SubmitEvent) {
		event.preventDefault();
		if (!canSave) return;
		busy = true;
		try {
			const url = baseUrl.trim();
			const primary = primaryModel.trim();
			const fast = fastModel.trim() || null;
			const secret = apiKey.trim();
			if (provider) {
				// Empty base_url clears it (the server then rejects that for openai_compatible).
				await api('PATCH', `/ai-providers/${provider.id}`, {
					name: name.trim(),
					base_url: url,
					primary_model: primary,
					fast_model: fast
				});
				// A key typed here (first key, or re-entered for a new base URL) is saved too.
				if (secret) {
					await api('POST', `/ai-providers/${provider.id}/keys`, {
						label: provider.keys.length === 0 ? 'Default' : `Key ${provider.keys.length + 1}`,
						key: secret
					});
				}
			} else {
				await api('POST', '/ai-providers', {
					kind,
					name: name.trim(),
					base_url: url || null,
					primary_model: primary,
					fast_model: fast,
					key: secret,
					key_label: 'Default'
				});
			}
			open = false;
			toast.success(provider ? 'Provider updated' : 'Provider added');
			onsaved();
		} catch (e) {
			toast.error(e instanceof ApiError ? e.message : 'Could not reach the server');
		} finally {
			busy = false;
		}
	}
</script>

<Dialog.Root bind:open>
	<Dialog.Content>
		<Dialog.Header>
			<Dialog.Title>{provider ? `Edit ${provider.name}` : 'Add provider'}</Dialog.Title>
			<Dialog.Description>
				Test the provider connection first to load available models.
			</Dialog.Description>
		</Dialog.Header>
		<form class="grid gap-4" onsubmit={submit}>
			{#if !provider}
				<div class="grid gap-2">
					<Label>Kind</Label>
					<Select.Root type="single" bind:value={kind} onValueChange={resetConnection}>
						<Select.Trigger class="w-full">{kindLabel[kind]}</Select.Trigger>
						<Select.Content>
							{#each Object.entries(kindLabel) as [value, label] (value)}
								<Select.Item {value} {label}>{label}</Select.Item>
							{/each}
						</Select.Content>
					</Select.Root>
				</div>
			{/if}
			<div class="grid gap-2">
				<Label for="p-name">Name</Label>
				<Input id="p-name" required bind:value={name} />
			</div>
			{#if shownKind === 'openai_compatible'}
				<div class="grid gap-2">
					<Label for="p-url">Base URL</Label>
					<Input
						id="p-url"
						type="url"
						required
						placeholder="https://host/v1"
						bind:value={baseUrl}
						oninput={resetConnection}
					/>
				</div>
			{/if}
			<div class="grid gap-2">
				<Label for="p-key">
					API key
					{#if !needsKey}
						<span class="text-xs font-normal text-muted-foreground">(optional)</span>
					{/if}
				</Label>
				<div class="flex gap-2">
					<Input
						id="p-key"
						type="password"
						autocomplete="off"
						required={needsKey}
						placeholder={urlChanged
							? 'Enter the key again for the new URL'
							: needsKey
								? 'sk-...'
								: 'Leave blank to use stored key'}
						bind:value={apiKey}
						oninput={resetConnection}
					/>
					<Button type="button" variant="outline" disabled={!canTest} onclick={testConnection}>
						{#if testing}
							<RiLoader3Line class="size-4 animate-spin" aria-hidden="true" />
						{/if}
						Test connection
					</Button>
				</div>
			</div>
			<div class="grid gap-2">
				<Label>Primary model</Label>
				<Popover.Root bind:open={primaryOpen}>
					<Popover.Trigger disabled={!tested}>
						{#snippet child({ props })}
							<Button
								{...props}
								variant="outline"
								role="combobox"
								aria-expanded={primaryOpen}
								disabled={!tested}
								class="w-full justify-between rounded-sm font-normal"
							>
								{#if primaryModel}
									<span class="truncate font-mono">{primaryModel}</span>
								{:else}
									<span class="text-muted-foreground">
										{tested ? 'Select primary model...' : 'Test connection to load models'}
									</span>
								{/if}
								<RiArrowDownSLine data-icon="inline-end" class="text-muted-foreground" />
							</Button>
						{/snippet}
					</Popover.Trigger>
					<Popover.Content class="w-(--bits-popover-anchor-width) p-0" align="start">
						<Command.Root>
							<Command.Input placeholder="Search or type a model..." bind:value={primarySearch} />
							<Command.List>
								<Command.Empty>No matching models.</Command.Empty>
								{#if customPrimary}
									<Command.Item
										value={customPrimary}
										data-checked={primaryModel === customPrimary}
										onSelect={() => {
											primaryModel = customPrimary;
											primaryOpen = false;
										}}
									>
										<span class="truncate">
											Use "<code class="font-mono">{customPrimary}</code>"
										</span>
									</Command.Item>
								{/if}
								{#each models as m (m)}
									<Command.Item
										value={m}
										data-checked={primaryModel === m}
										onSelect={() => {
											primaryModel = m;
											primaryOpen = false;
										}}
									>
										<span class="truncate font-mono">{m}</span>
									</Command.Item>
								{/each}
							</Command.List>
						</Command.Root>
					</Popover.Content>
				</Popover.Root>
			</div>
			<div class="grid gap-2">
				<Label>
					Fast model
					<span class="text-xs font-normal text-muted-foreground">(optional)</span>
				</Label>
				<Popover.Root bind:open={fastOpen}>
					<Popover.Trigger disabled={!tested}>
						{#snippet child({ props })}
							<Button
								{...props}
								variant="outline"
								role="combobox"
								aria-expanded={fastOpen}
								disabled={!tested}
								class="w-full justify-between rounded-sm font-normal"
							>
								{#if fastModel}
									<span class="truncate font-mono">{fastModel}</span>
								{:else}
									<span class="text-muted-foreground">
										{tested ? 'None (use Primary model)' : 'Test connection to load models'}
									</span>
								{/if}
								<RiArrowDownSLine data-icon="inline-end" class="text-muted-foreground" />
							</Button>
						{/snippet}
					</Popover.Trigger>
					<Popover.Content class="w-(--bits-popover-anchor-width) p-0" align="start">
						<Command.Root>
							<Command.Input placeholder="Search or type a model..." bind:value={fastSearch} />
							<Command.List>
								<Command.Empty>No matching models.</Command.Empty>
								<Command.Item
									value="None (use Primary model)"
									data-checked={!fastModel}
									onSelect={() => {
										fastModel = '';
										fastOpen = false;
									}}
								>
									<span>None (use Primary model)</span>
								</Command.Item>
								{#if customFast}
									<Command.Item
										value={customFast}
										data-checked={fastModel === customFast}
										onSelect={() => {
											fastModel = customFast;
											fastOpen = false;
										}}
									>
										<span class="truncate">
											Use "<code class="font-mono">{customFast}</code>"
										</span>
									</Command.Item>
								{/if}
								{#each models as m (m)}
									<Command.Item
										value={m}
										data-checked={fastModel === m}
										onSelect={() => {
											fastModel = m;
											fastOpen = false;
										}}
									>
										<span class="truncate font-mono">{m}</span>
									</Command.Item>
								{/each}
							</Command.List>
						</Command.Root>
					</Popover.Content>
				</Popover.Root>
			</div>
			<Dialog.Footer>
				<Button type="submit" disabled={!canSave}>{provider ? 'Save' : 'Add'}</Button>
			</Dialog.Footer>
		</form>
	</Dialog.Content>
</Dialog.Root>
