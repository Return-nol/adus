<script lang="ts" module>
	export type Memory = {
		id: string;
		content: string;
		repository_id: string | null;
		source: 'manual' | 'pr_comment' | 'pr_learning';
		source_url: string | null;
		created_by: { id: string; email: string } | null;
		enabled: boolean;
		status: 'pending' | 'active' | 'rejected';
	};
</script>

<script lang="ts">
	import { toast } from 'svelte-sonner';
	import { api, ApiError } from '#lib/api.js';
	import { Button } from '#lib/components/ui/button/index.js';
	import * as Dialog from '#lib/components/ui/dialog/index.js';
	import { Label } from '#lib/components/ui/label/index.js';
	import { Textarea } from '#lib/components/ui/textarea/index.js';

	// `memory` set = edit its content (`approve`: edit a pending one and approve it);
	// null = create one in `repositoryId` (null = global).
	let {
		open = $bindable(false),
		memory,
		repositoryId,
		approve = false,
		onsaved
	}: {
		open: boolean;
		memory: Memory | null;
		repositoryId: string | null;
		approve?: boolean;
		onsaved: () => void;
	} = $props();

	let content = $state('');
	let busy = $state(false);
	let formError = $state('');

	// Reset the form each time the dialog opens.
	$effect(() => {
		if (!open) return;
		content = memory?.content ?? '';
		formError = '';
	});

	async function submit(event: SubmitEvent) {
		event.preventDefault();
		busy = true;
		formError = '';
		try {
			if (memory && approve) {
				await api('POST', `/memories/${memory.id}/approve`, { content });
			} else if (memory) {
				await api('PATCH', `/memories/${memory.id}`, { content });
			} else {
				await api('POST', '/memories', {
					content,
					scope: repositoryId ? 'repo' : 'global',
					repository_id: repositoryId
				});
			}
			open = false;
			toast.success(approve ? 'Memory approved' : memory ? 'Memory updated' : 'Memory added');
			onsaved();
		} catch (e) {
			if (e instanceof ApiError && e.code === 'validation_error') formError = e.message;
			else toast.error(e instanceof ApiError ? e.message : 'Could not reach the server');
		} finally {
			busy = false;
		}
	}
</script>

<Dialog.Root bind:open>
	<Dialog.Content>
		<Dialog.Header>
			<Dialog.Title>
				{approve ? 'Edit and approve memory' : memory ? 'Edit memory' : 'Add memory'}
			</Dialog.Title>
		</Dialog.Header>
		<form class="grid gap-4" onsubmit={submit}>
			<div class="grid gap-2">
				<Label for="m-content">Content</Label>
				<Textarea id="m-content" required maxlength={2000} bind:value={content} />
			</div>
			{#if formError}<p role="alert" class="text-sm text-destructive">{formError}</p>{/if}
			<Dialog.Footer>
				<Button type="submit" disabled={busy}>
					{approve ? 'Approve' : memory ? 'Save' : 'Add'}
				</Button>
			</Dialog.Footer>
		</form>
	</Dialog.Content>
</Dialog.Root>
