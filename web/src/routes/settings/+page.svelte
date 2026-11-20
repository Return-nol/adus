<script lang="ts">
	import { onMount } from 'svelte';
	import { toast } from 'svelte-sonner';
	import { api, ApiError } from '#lib/api.js';
	import { session } from '#lib/session.svelte.js';
	import type { Provider } from '#lib/components/provider-dialog.svelte';
	import * as AlertDialog from '#lib/components/ui/alert-dialog/index.js';
	import { Badge } from '#lib/components/ui/badge/index.js';
	import { Button } from '#lib/components/ui/button/index.js';
	import * as Card from '#lib/components/ui/card/index.js';
	import { Input } from '#lib/components/ui/input/index.js';
	import { Label } from '#lib/components/ui/label/index.js';
	import * as Select from '#lib/components/ui/select/index.js';
	import { Textarea } from '#lib/components/ui/textarea/index.js';

	type Settings = {
		default_provider_id: string | null;
		default_model: string | null;
		max_files_per_review: number;
		max_patch_lines_per_file: number;
		ignore_paths: string[];
		ignore_paths_is_default: boolean;
		ignored_title_keywords: string[];
	};

	const isAdmin = $derived(session.user?.role === 'admin');
	let providers = $state<Provider[]>([]);
	let loaded = $state(false);
	let busy = $state(false);
	// '' = no default provider.
	let providerId = $state('');
	let model = $state('');
	let maxFiles = $state(0);
	let maxPatchLines = $state(0);
	// Ignored files: the textarea text, plus what the server last gave us.
	let ignoreText = $state('');
	let savedIgnore = $state<string[]>([]);
	let isDefault = $state(false);
	let keywordsText = $state('');
	let ignoreError = $state('');
	let resetOpen = $state(false);
	let testPath = $state('');
	let testing = $state(false);
	let testResult = $state<{ ignored: boolean; pattern: string | null } | null>(null);

	// One entry per line: trimmed, empties and duplicates dropped (the server does the same).
	function lines(text: string): string[] {
		return [
			...new Set(
				text
					.split(/\r?\n/)
					.map((l) => l.trim())
					.filter(Boolean)
			)
		];
	}
	const patterns = $derived(lines(ignoreText));
	const ignoreChanged = $derived(
		patterns.length !== savedIgnore.length || patterns.some((p, i) => p !== savedIgnore[i])
	);

	function fail(e: unknown) {
		toast.error(e instanceof ApiError ? e.message : 'Could not reach the server');
	}

	function fillIgnore(s: Settings) {
		savedIgnore = s.ignore_paths;
		ignoreText = s.ignore_paths.join('\n');
		isDefault = s.ignore_paths_is_default;
		ignoreError = '';
		testResult = null;
	}

	function fill(s: Settings) {
		fillIgnore(s);
		keywordsText = s.ignored_title_keywords.join('\n');
		providerId = s.default_provider_id ?? '';
		model = s.default_model ?? '';
		maxFiles = s.max_files_per_review;
		maxPatchLines = s.max_patch_lines_per_file;
	}

	onMount(async () => {
		if (!isAdmin) return;
		try {
			const [p, s] = await Promise.all([
				api<Provider[]>('GET', '/ai-providers'),
				api<Settings>('GET', '/settings')
			]);
			providers = p;
			fill(s);
			loaded = true;
		} catch (e) {
			fail(e);
		}
	});

	async function save(event: SubmitEvent) {
		event.preventDefault();
		busy = true;
		ignoreError = '';
		try {
			fill(
				await api<Settings>('PUT', '/settings', {
					default_provider_id: providerId || null,
					default_model: model.trim() || null,
					max_files_per_review: maxFiles,
					max_patch_lines_per_file: maxPatchLines,
					// Omitted unless edited, so saving other settings never pins the defaults.
					...(ignoreChanged ? { ignore_paths: patterns } : {}),
					ignored_title_keywords: lines(keywordsText)
				})
			);
			toast.success('Settings saved');
		} catch (e) {
			if (e instanceof ApiError && e.code === 'invalid_glob') ignoreError = e.message;
			else fail(e); // e.g. 422 default_model required
		} finally {
			busy = false;
		}
	}

	// Still on the defaults with only local edits: just refill them, nothing to save.
	function startReset() {
		if (isDefault) {
			ignoreText = savedIgnore.join('\n');
			ignoreError = '';
			testResult = null;
		} else {
			resetOpen = true;
		}
	}

	async function resetIgnore() {
		busy = true;
		try {
			// The PUT needs the other fields; send the saved ones and take only the ignore list
			// from the reply, so unsaved edits elsewhere on the page survive.
			const saved = await api<Settings>('GET', '/settings');
			fillIgnore(
				await api<Settings>('PUT', '/settings', {
					default_provider_id: saved.default_provider_id,
					default_model: saved.default_model,
					max_files_per_review: saved.max_files_per_review,
					max_patch_lines_per_file: saved.max_patch_lines_per_file,
					ignore_paths: null
				})
			);
			toast.success('Ignore list reset to defaults');
		} catch (e) {
			fail(e);
		} finally {
			busy = false;
		}
	}

	async function testFile() {
		if (!testPath.trim()) return;
		testing = true;
		try {
			testResult = await api<{ ignored: boolean; pattern: string | null }>(
				'POST',
				'/settings/ignore-test',
				{ path: testPath }
			);
		} catch (e) {
			testResult = null;
			fail(e);
		} finally {
			testing = false;
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
{:else if loaded}
	<Card.Root class="max-w-2xl">
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
				<div class="grid gap-2">
					<div class="flex flex-wrap items-center gap-2">
						<Label for="s-ignore">Ignored files</Label>
						{#if isDefault && !ignoreChanged}<Badge variant="secondary">Using defaults</Badge>{/if}
						<span class="text-xs text-muted-foreground">
							{patterns.length}
							{patterns.length === 1 ? 'pattern' : 'patterns'}
						</span>
						<Button
							type="button"
							variant="outline"
							size="sm"
							class="ml-auto"
							disabled={busy || (isDefault && !ignoreChanged)}
							onclick={startReset}
						>
							Reset to defaults
						</Button>
					</div>
					<Textarea
						id="s-ignore"
						rows={12}
						spellcheck={false}
						class="resize-y font-mono"
						aria-invalid={ignoreError ? true : undefined}
						bind:value={ignoreText}
					/>
					{#if ignoreError}
						<p class="text-sm text-destructive" role="alert">{ignoreError}</p>
					{/if}
					<p class="text-xs text-muted-foreground">
						One glob pattern per line. Matching files are skipped before the AI sees them.
					</p>
				</div>
				<div class="grid gap-2">
					<Label for="s-test">Test a file</Label>
					<div class="flex gap-2">
						<Input
							id="s-test"
							class="font-mono"
							placeholder="src/generated/api.ts"
							bind:value={testPath}
							onkeydown={(e) => {
								if (e.key === 'Enter') {
									e.preventDefault();
									testFile();
								}
							}}
						/>
						<Button
							type="button"
							variant="outline"
							disabled={testing || !testPath.trim()}
							onclick={testFile}
						>
							Test
						</Button>
					</div>
					{#if testResult}
						<p class="text-sm" aria-live="polite">
							{#if testResult.ignored}
								Ignored by <code class="font-mono">{testResult.pattern}</code>
							{:else}
								Will be reviewed
							{/if}
						</p>
					{/if}
					<p class="text-xs text-muted-foreground">Tests the saved list, not unsaved edits.</p>
				</div>
				<div class="grid gap-2">
					<Label for="s-keywords">Ignored title keywords</Label>
					<Textarea id="s-keywords" rows={3} bind:value={keywordsText} />
					<p class="text-xs text-muted-foreground">
						One per line. Pull requests whose title contains one (any case) are not reviewed
						automatically.
					</p>
				</div>
				<div><Button type="submit" disabled={busy}>Save</Button></div>
			</form>
		</Card.Content>
	</Card.Root>
{/if}

<AlertDialog.Root bind:open={resetOpen}>
	<AlertDialog.Content>
		<AlertDialog.Header>
			<AlertDialog.Title>Reset to defaults?</AlertDialog.Title>
			<AlertDialog.Description>
				Replace your list with the default patterns?
			</AlertDialog.Description>
		</AlertDialog.Header>
		<AlertDialog.Footer>
			<AlertDialog.Cancel>Cancel</AlertDialog.Cancel>
			<AlertDialog.Action onclick={resetIgnore}>Reset</AlertDialog.Action>
		</AlertDialog.Footer>
	</AlertDialog.Content>
</AlertDialog.Root>
