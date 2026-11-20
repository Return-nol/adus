<script lang="ts" module>
	export type Severity = 'info' | 'warning' | 'critical';
	export type Rule = {
		id: string;
		title: string;
		body: string;
		path_glob: string | null;
		severity: Severity;
		enabled: boolean;
	};
	export const severityLabel: Record<Severity, string> = {
		info: 'Info',
		warning: 'Warning',
		critical: 'Critical'
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
	import { Textarea } from '#lib/components/ui/textarea/index.js';

	// `rule` set = edit it; null = create one in `repositoryId` (null = global).
	let {
		open = $bindable(false),
		rule,
		repositoryId,
		onsaved
	}: {
		open: boolean;
		rule: Rule | null;
		repositoryId: string | null;
		onsaved: () => void;
	} = $props();

	let title = $state('');
	let severity = $state<Severity>('warning');
	let pathGlob = $state('');
	let body = $state('');
	let busy = $state(false);
	let globError = $state('');
	let formError = $state('');

	// Reset the form each time the dialog opens.
	$effect(() => {
		if (!open) return;
		title = rule?.title ?? '';
		severity = rule?.severity ?? 'warning';
		pathGlob = rule?.path_glob ?? '';
		body = rule?.body ?? '';
		globError = '';
		formError = '';
	});

	async function submit(event: SubmitEvent) {
		event.preventDefault();
		busy = true;
		globError = '';
		formError = '';
		try {
			// An empty glob clears it on edit and means "all files" on create.
			const fields = { title, body, severity, path_glob: pathGlob.trim() };
			if (rule) {
				await api('PATCH', `/rules/${rule.id}`, fields);
			} else {
				await api('POST', '/rules', {
					...fields,
					scope: repositoryId ? 'repo' : 'global',
					repository_id: repositoryId
				});
			}
			open = false;
			toast.success(rule ? 'Rule updated' : 'Rule added');
			onsaved();
		} catch (e) {
			if (e instanceof ApiError && e.code === 'invalid_glob') globError = e.message;
			else if (e instanceof ApiError && e.code === 'validation_error') formError = e.message;
			else toast.error(e instanceof ApiError ? e.message : 'Could not reach the server');
		} finally {
			busy = false;
		}
	}
</script>

<Dialog.Root bind:open>
	<Dialog.Content>
		<Dialog.Header>
			<Dialog.Title>{rule ? `Edit ${rule.title}` : 'Add rule'}</Dialog.Title>
		</Dialog.Header>
		<form class="grid gap-4" onsubmit={submit}>
			<div class="grid gap-2">
				<Label for="r-title">Title</Label>
				<Input id="r-title" required maxlength={200} bind:value={title} />
			</div>
			<div class="grid gap-2">
				<Label>Severity</Label>
				<Select.Root type="single" bind:value={severity}>
					<Select.Trigger class="w-full">{severityLabel[severity]}</Select.Trigger>
					<Select.Content>
						{#each Object.entries(severityLabel) as [value, label] (value)}
							<Select.Item {value} {label}>{label}</Select.Item>
						{/each}
					</Select.Content>
				</Select.Root>
			</div>
			<div class="grid gap-2">
				<Label for="r-glob">Path glob</Label>
				<Input
					id="r-glob"
					maxlength={500}
					placeholder="src/**/*.rs"
					aria-invalid={globError ? true : undefined}
					bind:value={pathGlob}
				/>
				<p class="text-xs text-muted-foreground">e.g. src/**/*.rs — leave empty for all files</p>
				{#if globError}<p role="alert" class="text-sm text-destructive">{globError}</p>{/if}
			</div>
			<div class="grid gap-2">
				<Label for="r-body">Rule</Label>
				<Textarea id="r-body" required rows={6} maxlength={10000} bind:value={body} />
			</div>
			{#if formError}<p role="alert" class="text-sm text-destructive">{formError}</p>{/if}
			<Dialog.Footer>
				<Button type="submit" disabled={busy}>{rule ? 'Save' : 'Add'}</Button>
			</Dialog.Footer>
		</form>
	</Dialog.Content>
</Dialog.Root>
