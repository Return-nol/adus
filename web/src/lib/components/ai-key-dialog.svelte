<script lang="ts">
	import { toast } from 'svelte-sonner';
	import { api, ApiError } from '#lib/api.js';
	import { Button } from '#lib/components/ui/button/index.js';
	import * as Dialog from '#lib/components/ui/dialog/index.js';
	import { Input } from '#lib/components/ui/input/index.js';
	import { Label } from '#lib/components/ui/label/index.js';
	import type { AiKey } from './provider-dialog.svelte';

	// `aiKey` set = edit that key (blank key field keeps the stored key); null = add a key to `providerId`.
	let {
		open = $bindable(false),
		providerId,
		aiKey,
		onsaved
	}: { open: boolean; providerId: string; aiKey: AiKey | null; onsaved: () => void } = $props();

	let label = $state('');
	let secret = $state('');
	let busy = $state(false);

	// Reset when the dialog opens; the secret is also dropped whenever it closes (saved, failed or cancelled).
	$effect(() => {
		if (!open) {
			secret = '';
			return;
		}
		label = aiKey?.label ?? '';
		secret = '';
	});

	async function submit(event: SubmitEvent) {
		event.preventDefault();
		busy = true;
		try {
			if (aiKey) {
				await api('PATCH', `/ai-keys/${aiKey.id}`, {
					label,
					...(secret ? { key: secret } : {})
				});
			} else {
				await api('POST', `/ai-providers/${providerId}/keys`, { label, key: secret });
			}
			open = false;
			toast.success(aiKey ? 'Key updated' : 'Key added');
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
			<Dialog.Title>{aiKey ? `Edit key ${aiKey.label}` : 'Add key'}</Dialog.Title>
			{#if aiKey}
				<Dialog.Description>
					Leave the key empty to keep the stored one. A new key resets the status and cooldown.
				</Dialog.Description>
			{/if}
		</Dialog.Header>
		<form class="grid gap-4" onsubmit={submit}>
			<div class="grid gap-2">
				<Label for="k-label">Label</Label>
				<Input id="k-label" required bind:value={label} />
			</div>
			<div class="grid gap-2">
				<Label for="k-key">API key</Label>
				<Input
					id="k-key"
					type="password"
					autocomplete="off"
					required={!aiKey}
					bind:value={secret}
				/>
			</div>
			<Dialog.Footer>
				<Button type="submit" disabled={busy}>{aiKey ? 'Save' : 'Add key'}</Button>
			</Dialog.Footer>
		</form>
	</Dialog.Content>
</Dialog.Root>
