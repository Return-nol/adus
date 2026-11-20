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
	import { api, ApiError } from '#lib/api.js';
	import { Button } from '#lib/components/ui/button/index.js';
	import * as Dialog from '#lib/components/ui/dialog/index.js';
	import { Input } from '#lib/components/ui/input/index.js';
	import { Label } from '#lib/components/ui/label/index.js';
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
	let busy = $state(false);

	// Reset the form each time the dialog opens.
	$effect(() => {
		if (!open) return;
		kind = provider?.kind ?? 'openai';
		name = provider?.name ?? '';
		baseUrl = provider?.base_url ?? '';
	});

	const shownKind = $derived(provider?.kind ?? kind);

	async function submit(event: SubmitEvent) {
		event.preventDefault();
		busy = true;
		try {
			const url = baseUrl.trim();
			if (provider) {
				// Empty base_url clears it (the server then rejects that for openai_compatible).
				await api('PATCH', `/ai-providers/${provider.id}`, { name, base_url: url });
			} else {
				await api('POST', '/ai-providers', { kind, name, base_url: url || null });
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
		</Dialog.Header>
		<form class="grid gap-4" onsubmit={submit}>
			{#if !provider}
				<div class="grid gap-2">
					<Label>Kind</Label>
					<Select.Root type="single" bind:value={kind}>
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
					/>
				</div>
			{/if}
			<Dialog.Footer>
				<Button type="submit" disabled={busy}>{provider ? 'Save' : 'Add'}</Button>
			</Dialog.Footer>
		</form>
	</Dialog.Content>
</Dialog.Root>
