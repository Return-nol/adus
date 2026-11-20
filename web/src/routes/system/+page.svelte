<script lang="ts">
	import { onMount } from 'svelte';
	import { toast } from 'svelte-sonner';
	import { api, ApiError } from '#lib/api.js';
	import { session } from '#lib/session.svelte.js';
	import { Button } from '#lib/components/ui/button/index.js';
	import * as Card from '#lib/components/ui/card/index.js';
	import RiCheckLine from 'remixicon-svelte/icons/check-line';
	import RiCloseCircleLine from 'remixicon-svelte/icons/close-circle-line';
	import RiErrorWarningLine from 'remixicon-svelte/icons/error-warning-line';

	type Status = 'ok' | 'warn' | 'fail';
	type Verdict = 'ok' | 'degraded' | 'not_running';
	type Check = { name: string; status: Status; impact: string | null; fix: string | null };
	type Report = { verdict: Verdict; checks: Check[] };

	// Semantic tokens only: ok = primary, degraded = outlined primary, not running = destructive.
	const verdictMeta: Record<Verdict, { label: string; hint: string; class: string }> = {
		ok: {
			label: 'Reviews: OK',
			hint: 'Everything needed to review pull requests is working.',
			class: 'border-transparent bg-primary text-primary-foreground'
		},
		degraded: {
			label: 'Reviews: Degraded',
			hint: 'Reviews run, but some repositories or connections have a problem.',
			class: 'border-primary bg-secondary text-foreground'
		},
		not_running: {
			label: 'Reviews: Not running',
			hint: 'No review can run until the failed checks below are fixed.',
			class: 'border-transparent bg-destructive/10 text-destructive dark:bg-destructive/15'
		}
	};
	const statusClass: Record<Status, string> = {
		ok: 'text-primary',
		warn: 'text-foreground',
		fail: 'text-destructive'
	};
	const statusLabel: Record<Status, string> = { ok: 'OK', warn: 'Warning', fail: 'Failed' };
	const order: Record<Status, number> = { fail: 0, warn: 1, ok: 2 };

	const isAdmin = $derived(session.user?.role === 'admin');
	let report = $state<Report | null>(null);
	let busy = $state(false);
	let loadError = $state(false);

	// Fail first, then warnings, then healthy checks; the server order is kept within a status.
	const checks = $derived(
		report ? [...report.checks].sort((a, b) => order[a.status] - order[b.status]) : []
	);

	async function run() {
		busy = true;
		try {
			report = await api<Report>('GET', '/admin/doctor');
			loadError = false;
		} catch (e) {
			loadError = report === null;
			toast.error(e instanceof ApiError ? e.message : 'Could not reach the server');
		} finally {
			busy = false;
		}
	}

	onMount(() => {
		if (isAdmin) run();
	});
</script>

<div class="mb-6 flex flex-wrap items-center justify-between gap-3">
	<h1 class="font-display text-3xl font-medium tracking-[0.01em] text-foreground">System status</h1>
	{#if isAdmin}
		<Button variant="outline" disabled={busy} onclick={run}>
			{busy ? 'Checking…' : 'Run again'}
		</Button>
	{/if}
</div>

{#if !isAdmin}
	<div
		class="rounded-xl border border-dashed border-border bg-card px-8 py-12 text-center text-sm text-muted-foreground"
	>
		Only admins can see the system status.
	</div>
{:else if loadError}
	<div
		class="rounded-xl border border-dashed border-border bg-card px-8 py-12 text-center text-sm text-muted-foreground"
	>
		Could not run the checks.
		<Button variant="outline" size="sm" class="ml-2" disabled={busy} onclick={run}>Retry</Button>
	</div>
{:else if report}
	{@const meta = verdictMeta[report.verdict]}
	<Card.Root class="mb-6">
		<Card.Content class="flex flex-wrap items-center gap-4">
			<span
				class={`inline-flex items-center rounded-full border px-5 py-2 font-display text-2xl font-medium ${meta.class}`}
			>
				{meta.label}
			</span>
			<p class="text-sm text-muted-foreground">{meta.hint}</p>
		</Card.Content>
	</Card.Root>

	<Card.Root>
		<Card.Content class="divide-y divide-border p-0">
			{#each checks as check (check.name)}
				<div class="flex gap-3 px-6 py-4">
					<span class={`mt-0.5 shrink-0 ${statusClass[check.status]}`}>
						{#if check.status === 'ok'}
							<RiCheckLine class="size-5" aria-label={statusLabel.ok} />
						{:else if check.status === 'warn'}
							<RiErrorWarningLine class="size-5" aria-label={statusLabel.warn} />
						{:else}
							<RiCloseCircleLine class="size-5" aria-label={statusLabel.fail} />
						{/if}
					</span>
					<div class="min-w-0">
						<p class="font-medium text-foreground">{check.name}</p>
						{#if check.impact}
							<p class="mt-1 text-sm text-muted-foreground">
								<span class="font-medium text-foreground">Impact:</span>
								{check.impact}
							</p>
						{/if}
						{#if check.fix}
							<p class="mt-1 text-sm text-muted-foreground">
								<span class="font-medium text-foreground">Fix:</span>
								{check.fix}
							</p>
						{/if}
					</div>
				</div>
			{/each}
		</Card.Content>
	</Card.Root>
{/if}
