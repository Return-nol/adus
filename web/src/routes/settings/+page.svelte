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
	import { Skeleton } from '#lib/components/ui/skeleton/index.js';

	type ReviewMode = 'generalist' | 'specialists';
	type CuratorEffort = 'low' | 'medium' | 'high';

	const reviewModeLabels: Record<ReviewMode, string> = {
		generalist: 'Generalist — balanced single-pass review',
		specialists: 'Specialists — parallel Bug, Security, Performance & Rules'
	};
	const curatorEffortLabels: Record<CuratorEffort, string> = {
		low: 'Low',
		medium: 'Medium',
		high: 'High'
	};

	type Settings = {
		default_provider_id: string | null;
		review_mode: ReviewMode;
		max_files_per_review: number;
		max_patch_lines_per_file: number;
		small_pr_max_lines: number;
		max_review_input_tokens: number;
		ignore_paths: string[];
		ignore_paths_is_default: boolean;
		ignored_title_keywords: string[];
		max_thread_replies: number;
		max_validate_threads: number;
		max_learned_per_pr: number;
		curator_effort: CuratorEffort;
	};

	const isAdmin = $derived(session.user?.role === 'admin');
	let providers = $state<Provider[]>([]);
	let loaded = $state(false);
	let loadError = $state(false);
	let busy = $state(false);
	// '' = no default provider.
	let providerId = $state('');
	let reviewMode = $state<ReviewMode>('generalist');
	let maxFiles = $state(0);
	let maxPatchLines = $state(0);
	let smallPrMaxLines = $state(0);
	let maxInputTokens = $state(0);
	let maxThreadReplies = $state(5);
	let maxValidateThreads = $state(30);
	let maxLearnedPerPr = $state(10);
	let curatorEffort = $state<CuratorEffort>('medium');
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
		reviewMode = s.review_mode;
		maxFiles = s.max_files_per_review;
		maxPatchLines = s.max_patch_lines_per_file;
		smallPrMaxLines = s.small_pr_max_lines;
		maxInputTokens = s.max_review_input_tokens;
		maxThreadReplies = s.max_thread_replies;
		maxValidateThreads = s.max_validate_threads;
		maxLearnedPerPr = s.max_learned_per_pr;
		curatorEffort = s.curator_effort;
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
		ignoreError = '';
		try {
			fill(
				await api<Settings>('PUT', '/settings', {
					default_provider_id: providerId || null,
					review_mode: reviewMode,
					max_files_per_review: maxFiles,
					max_patch_lines_per_file: maxPatchLines,
					small_pr_max_lines: smallPrMaxLines,
					max_review_input_tokens: maxInputTokens,
					max_thread_replies: maxThreadReplies,
					max_validate_threads: maxValidateThreads,
					max_learned_per_pr: maxLearnedPerPr,
					curator_effort: curatorEffort,
					// Omitted unless edited, so saving other settings never pins the defaults.
					...(ignoreChanged ? { ignore_paths: patterns } : {}),
					ignored_title_keywords: lines(keywordsText)
				})
			);
			toast.success('Settings saved');
		} catch (e) {
			if (e instanceof ApiError && e.code === 'invalid_glob') ignoreError = e.message;
			else fail(e);
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
{:else if loadError}
	<div
		class="rounded-xl border border-dashed border-border bg-card px-8 py-12 text-center text-sm text-muted-foreground"
	>
		Could not load settings.
		<Button variant="outline" size="sm" class="ml-2" onclick={load}>Retry</Button>
	</div>
{:else if !loaded}
	<div class="grid gap-6" aria-busy="true">
		<span class="sr-only" role="status">Loading…</span>
		<Card.Root class="max-w-2xl">
			<Card.Content class="grid gap-4">
				{#each ['w-28', 'w-24', 'w-44', 'w-52', 'w-56', 'w-52', 'w-48', 'w-24', 'w-44'] as w, i (i)}
					<div class="grid gap-2">
						<Skeleton class={`h-4 ${w}`} />
						<Skeleton class="h-9 w-full rounded-sm" />
					</div>
				{/each}
			</Card.Content>
		</Card.Root>
		<Card.Root class="max-w-2xl">
			<Card.Header>
				<Skeleton class="h-5 w-56" />
			</Card.Header>
			<Card.Content class="grid gap-4">
				{#each ['w-48', 'w-56', 'w-52', 'w-44'] as w, i (i)}
					<div class="grid gap-2">
						<Skeleton class={`h-4 ${w}`} />
						<Skeleton class="h-9 w-full rounded-sm" />
					</div>
				{/each}
			</Card.Content>
		</Card.Root>
		<Skeleton class="h-9 w-16 rounded-lg" />
	</div>
{:else}
	<form class="grid gap-6" onsubmit={save}>
		<Card.Root class="max-w-2xl">
			<Card.Content class="grid gap-4">
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
					<Label>Review mode</Label>
					<Select.Root type="single" bind:value={reviewMode}>
						<Select.Trigger class="w-full" aria-label="Review mode">
							{reviewModeLabels[reviewMode]}
						</Select.Trigger>
						<Select.Content>
							{#each Object.entries(reviewModeLabels) as [value, label] (value)}
								<Select.Item {value} {label}>{label}</Select.Item>
							{/each}
						</Select.Content>
					</Select.Root>
					<p class="text-xs text-muted-foreground">
						Repositories can override this. <code class="font-mono">@adudu review deep</code> runs the
						specialists on any pull request.
					</p>
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
					<Label for="s-small">Small PR threshold (changed lines)</Label>
					<Input
						id="s-small"
						type="number"
						min="0"
						max="10000"
						required
						bind:value={smallPrMaxLines}
					/>
					<p class="text-xs text-muted-foreground">
						PRs with at most this many changed lines in up to 3 files, outside sensitive paths, are
						reviewed from the diff only, in one Generalist pass. 0 = always full review.
						<code class="font-mono">@adudu review</code> always runs the full review.
					</p>
				</div>
				<div class="grid gap-2">
					<Label for="s-tokens">Max input tokens per review (0-100000000)</Label>
					<Input
						id="s-tokens"
						type="number"
						min="0"
						max="100000000"
						required
						bind:value={maxInputTokens}
					/>
					<p class="text-xs text-muted-foreground">
						Input tokens one review may send to the model, cached tokens included. When reached,
						reviewers stop reading the repository, remaining findings are verified from the diff
						only, and the review posts what it has. 0 = no limit.
					</p>
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
			</Card.Content>
		</Card.Root>
		<Card.Root class="max-w-2xl">
			<Card.Header>
				<Card.Title>Thread conversations &amp; learning</Card.Title>
			</Card.Header>
			<Card.Content class="grid gap-4">
				<div class="grid gap-2">
					<Label for="s-thread-replies">Max replies per thread (0-100)</Label>
					<Input
						id="s-thread-replies"
						type="number"
						min="0"
						max="100"
						required
						bind:value={maxThreadReplies}
					/>
					<p class="text-xs text-muted-foreground">
						Maximum Adudu replies per PR review thread. 0 disables thread replies.
					</p>
				</div>
				<div class="grid gap-2">
					<Label for="s-validate-threads">Max threads validated per push (0-100)</Label>
					<Input
						id="s-validate-threads"
						type="number"
						min="0"
						max="100"
						required
						bind:value={maxValidateThreads}
					/>
					<p class="text-xs text-muted-foreground">
						Maximum open finding threads re-checked when new commits are pushed. 0 disables push
						validation.
					</p>
				</div>
				<div class="grid gap-2">
					<Label for="s-learned-pr">Max learned memories per PR (1-50)</Label>
					<Input
						id="s-learned-pr"
						type="number"
						min="1"
						max="50"
						required
						bind:value={maxLearnedPerPr}
					/>
					<p class="text-xs text-muted-foreground">
						Maximum curated memories proposed when a pull request closes.
					</p>
				</div>
				<div class="grid gap-2">
					<Label>Curator reasoning effort</Label>
					<Select.Root type="single" bind:value={curatorEffort}>
						<Select.Trigger class="w-full" aria-label="Curator reasoning effort">
							{curatorEffortLabels[curatorEffort]}
						</Select.Trigger>
						<Select.Content>
							{#each Object.entries(curatorEffortLabels) as [value, label] (value)}
								<Select.Item {value} {label}>{label}</Select.Item>
							{/each}
						</Select.Content>
					</Select.Root>
				</div>
			</Card.Content>
		</Card.Root>
		<div><Button type="submit" disabled={busy}>Save</Button></div>
	</form>
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
