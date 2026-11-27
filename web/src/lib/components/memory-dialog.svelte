<script lang="ts" module>
	export type MemoryKind = 'convention' | 'exception' | 'fix_pattern';
	export type MemoryEvidence = {
		url: string;
		pr_number: number | null;
	};
	export type Memory = {
		id: string;
		scope: 'global' | 'repo';
		content: string;
		repository_id: string | null;
		source: 'manual' | 'pr_comment' | 'pr_learning';
		source_url: string | null;
		created_by: { id: string; email: string } | null;
		enabled: boolean;
		status: 'pending' | 'active' | 'rejected';
		kind: MemoryKind;
		context: string | null;
		reason: string | null;
		resolution: string | null;
		evidence: MemoryEvidence[];
		supersedes: string | null;
		superseded_by: string | null;
		created_at: string;
	};
	export const kindLabel: Record<MemoryKind, string> = {
		convention: 'Convention',
		exception: 'Exception',
		fix_pattern: 'Fix pattern'
	};
</script>

<script lang="ts">
	import { toast } from 'svelte-sonner';
	import { api, ApiError } from '#lib/api.js';
	import { Button } from '#lib/components/ui/button/index.js';
	import * as Dialog from '#lib/components/ui/dialog/index.js';
	import { Label } from '#lib/components/ui/label/index.js';
	import * as Select from '#lib/components/ui/select/index.js';
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
	let kind = $state<MemoryKind>('convention');
	let context = $state('');
	let reason = $state('');
	let resolution = $state('');
	let busy = $state(false);
	let formError = $state('');

	// Reset the form each time the dialog opens.
	$effect(() => {
		if (!open) return;
		content = memory?.content ?? '';
		kind = memory?.kind ?? 'convention';
		context = memory?.context ?? '';
		reason = memory?.reason ?? '';
		resolution = memory?.resolution ?? '';
		formError = '';
	});

	function optionalText(value: string): string | null {
		const trimmed = value.trim();
		return trimmed.length > 0 ? trimmed : null;
	}

	async function submit(event: SubmitEvent) {
		event.preventDefault();
		busy = true;
		formError = '';
		const nextContext = optionalText(context);
		const nextReason = optionalText(reason);
		const nextResolution = optionalText(resolution);
		try {
			if (memory && approve) {
				const curatedChanged =
					kind !== memory.kind ||
					nextContext !== (memory.context ?? null) ||
					nextReason !== (memory.reason ?? null) ||
					nextResolution !== (memory.resolution ?? null);
				if (curatedChanged) {
					await api('PATCH', `/memories/${memory.id}`, {
						kind,
						context: nextContext,
						reason: nextReason,
						resolution: nextResolution
					});
				}
				await api('POST', `/memories/${memory.id}/approve`, { content });
			} else if (memory) {
				await api('PATCH', `/memories/${memory.id}`, {
					content,
					kind,
					context: nextContext,
					reason: nextReason,
					resolution: nextResolution
				});
			} else {
				await api('POST', '/memories', {
					content,
					scope: repositoryId ? 'repo' : 'global',
					repository_id: repositoryId,
					kind,
					context: nextContext,
					reason: nextReason,
					resolution: nextResolution
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
	<Dialog.Content class="max-h-[90vh] overflow-y-auto">
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
			<div class="grid gap-2">
				<Label>Kind</Label>
				<Select.Root type="single" bind:value={kind}>
					<Select.Trigger class="w-full" aria-label="Kind">{kindLabel[kind]}</Select.Trigger>
					<Select.Content>
						{#each Object.entries(kindLabel) as [value, label] (value)}
							<Select.Item {value} {label}>{label}</Select.Item>
						{/each}
					</Select.Content>
				</Select.Root>
			</div>
			<div class="grid gap-2">
				<Label for="m-context">Context (optional)</Label>
				<Textarea id="m-context" rows={2} maxlength={2000} bind:value={context} />
			</div>
			<div class="grid gap-2">
				<Label for="m-reason">Reason (optional)</Label>
				<Textarea id="m-reason" rows={2} maxlength={2000} bind:value={reason} />
			</div>
			<div class="grid gap-2">
				<Label for="m-resolution">Resolution (optional)</Label>
				<Textarea id="m-resolution" rows={2} maxlength={2000} bind:value={resolution} />
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
