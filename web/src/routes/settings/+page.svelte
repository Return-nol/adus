<script lang="ts">
	import { onMount } from 'svelte';
	import { toast } from 'svelte-sonner';
	import { api, ApiError } from '#lib/api.js';
	import { session } from '#lib/session.svelte.js';
	import type { Provider } from '#lib/components/provider-dialog.svelte';
	import { Button } from '#lib/components/ui/button/index.js';
	import * as Card from '#lib/components/ui/card/index.js';
	import { Input } from '#lib/components/ui/input/index.js';
	import { Label } from '#lib/components/ui/label/index.js';
	import * as Select from '#lib/components/ui/select/index.js';
	import { Skeleton } from '#lib/components/ui/skeleton/index.js';

	type Settings = {
		default_provider_id: string | null;
		default_model: string | null;
		max_files_per_review: number;
		max_patch_lines_per_file: number;
	};

	const isAdmin = $derived(session.user?.role === 'admin');
	let providers = $state<Provider[]>([]);
	let loaded = $state(false);
	let loadError = $state(false);
	let busy = $state(false);
	// '' = no default provider.
	let providerId = $state('');
	let model = $state('');
	let maxFiles = $state(0);
	let maxPatchLines = $state(0);

	function fail(e: unknown) {
		toast.error(e instanceof ApiError ? e.message : 'Could not reach the server');
	}

	function fill(s: Settings) {
		providerId = s.default_provider_id ?? '';
		model = s.default_model ?? '';
		maxFiles = s.max_files_per_review;
		maxPatchLines = s.max_patch_lines_per_file;
	}

	async function load() {
		loadError = false;
		try {
			const [p, s] = await Promise.all([
				api<Provider[]>('GET', '/ai-providers'),
				api<Settings>('GET', '/settings')
			]);
			providers = p;
			fill(s);
			loaded = true;
		} catch (e) {
			loadError = true;
			fail(e);
		}
	}

	onMount(() => {
		if (isAdmin) load();
	});

	async function save(event: SubmitEvent) {
		event.preventDefault();
		busy = true;
		try {
			fill(
				await api<Settings>('PUT', '/settings', {
					default_provider_id: providerId || null,
					default_model: model.trim() || null,
					max_files_per_review: maxFiles,
					max_patch_lines_per_file: maxPatchLines
				})
			);
			toast.success('Settings saved');
		} catch (e) {
			fail(e); // e.g. 422 default_model required
		} finally {
			busy = false;
		}
	}
</script>

<h1 class="mb-6 font-display text-3xl font-medium tracking-[0.01em] text-foreground">Settings</h1>

{#if !isAdmin}
	<div
		class="rounded-xl border border-dashed border-border bg-card px-8 py-12 text-center text-sm text-muted-foreground"
	>
		Settings are for admins only.
	</div>
{:else if loadError}
	<div
		class="rounded-xl border border-dashed border-border bg-card px-8 py-12 text-center text-sm text-muted-foreground"
	>
		Could not load settings.
		<Button variant="outline" size="sm" class="ml-2" onclick={load}>Retry</Button>
	</div>
{:else if !loaded}
	<Card.Root class="max-w-xl" aria-busy="true">
		<Card.Content class="grid gap-4">
			<span class="sr-only" role="status">Loading…</span>
			{#each ['w-28', 'w-24', 'w-44', 'w-52'] as w, i (i)}
				<div class="grid gap-2">
					<Skeleton class={`h-4 ${w}`} />
					<Skeleton class="h-9 w-full rounded-sm" />
				</div>
			{/each}
			<Skeleton class="h-9 w-16 rounded-lg" />
		</Card.Content>
	</Card.Root>
{:else}
	<Card.Root class="max-w-xl">
		<Card.Content>
			<form class="grid gap-4" onsubmit={save}>
				<div class="grid gap-2">
					<Label>Default provider</Label>
					<Select.Root type="single" bind:value={providerId}>
						<Select.Trigger class="w-full">
							{providers.find((p) => p.id === providerId)?.name ?? 'None'}
						</Select.Trigger>
						<Select.Content>
							<Select.Item value="" label="None">None</Select.Item>
							{#each providers as p (p.id)}
								<Select.Item value={p.id} label={p.name}>{p.name}</Select.Item>
							{/each}
						</Select.Content>
					</Select.Root>
				</div>
				<div class="grid gap-2">
					<Label for="s-model">Default model</Label>
					<Input
						id="s-model"
						placeholder="Required when a default provider is set"
						bind:value={model}
					/>
				</div>
				<div class="grid gap-2">
					<Label for="s-files">Max files per review (1-500)</Label>
					<Input id="s-files" type="number" min="1" max="500" required bind:value={maxFiles} />
				</div>
				<div class="grid gap-2">
					<Label for="s-lines">Max patch lines per file (1-100000)</Label>
					<Input
						id="s-lines"
						type="number"
						min="1"
						max="100000"
						required
						bind:value={maxPatchLines}
					/>
				</div>
				<div><Button type="submit" disabled={busy}>Save</Button></div>
			</form>
		</Card.Content>
	</Card.Root>
{/if}
