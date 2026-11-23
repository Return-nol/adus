<script lang="ts">
	import { toast } from 'svelte-sonner';
	import { BarChart } from 'layerchart';
	import { DateFormatter, endOfMonth, startOfMonth, today } from '@internationalized/date';
	import type { DateRange } from 'bits-ui';
	import RiCalendarLine from 'remixicon-svelte/icons/calendar-line';
	import { api, ApiError } from '#lib/api.js';
	import { Button } from '#lib/components/ui/button/index.js';
	import * as Chart from '#lib/components/ui/chart/index.js';
	import * as Card from '#lib/components/ui/card/index.js';
	import { Label } from '#lib/components/ui/label/index.js';
	import * as Popover from '#lib/components/ui/popover/index.js';
	import { RangeCalendar } from '#lib/components/ui/range-calendar/index.js';
	import * as Table from '#lib/components/ui/table/index.js';
	import * as Tabs from '#lib/components/ui/tabs/index.js';
	import { Skeleton } from '#lib/components/ui/skeleton/index.js';
	import TableSkeleton from '#lib/components/table-skeleton.svelte';

	type Daily = {
		date: string;
		input_tokens: number;
		output_tokens: number;
		cache_read_tokens: number;
		calls: number;
		failed_calls: number;
	};
	type Row = {
		id?: string;
		label: string;
		input_tokens: number;
		output_tokens: number;
		cache_read_tokens: number;
		calls: number;
	};

	const now = today('UTC');
	const defaultStart = startOfMonth(now);
	const defaultEnd = endOfMonth(now);
	const df = new DateFormatter('en-US', { dateStyle: 'medium', timeZone: 'UTC' });

	let range = $state<DateRange | undefined>({
		start: defaultStart,
		end: defaultEnd
	});
	const from = $derived(range?.start?.toString().slice(0, 10));
	const to = $derived(range?.end?.toString().slice(0, 10));
	let by = $state('repository');
	let daily = $state<Daily[]>([]);
	let rows = $state<Row[]>([]);
	let loading = $state(false);
	// False until the first load finishes; later reloads keep showing the old data.
	let loaded = $state(false);

	const chartConfig = {
		input_tokens: { label: 'Input', color: 'var(--chart-2)' },
		output_tokens: { label: 'Output', color: 'var(--chart-1)' }
	} satisfies Chart.ChartConfig;
	const series = [
		{ key: 'input_tokens', label: 'Input', color: chartConfig.input_tokens.color },
		{ key: 'output_tokens', label: 'Output', color: chartConfig.output_tokens.color }
	];

	const n = (v: number) => v.toLocaleString();
	const sum = (f: (d: Daily) => number) => daily.reduce((acc, d) => acc + f(d), 0);

	// Reload when the range or the tab changes; a slower older request never overwrites a newer one.
	$effect(() => {
		if (!from || !to) return;
		const query = `from=${from}&to=${to}`;
		const tab = by;
		let stale = false;
		loading = true;
		(async () => {
			try {
				const [d, r] = await Promise.all([
					api<Daily[]>('GET', `/usage/daily?${query}`),
					api<Row[]>('GET', `/usage/breakdown?${query}&by=${tab}`)
				]);
				if (stale) return;
				daily = d;
				rows = r;
			} catch (e) {
				if (!stale) toast.error(e instanceof ApiError ? e.message : 'Could not reach the server');
			}
			if (!stale) {
				loading = false;
				loaded = true;
			}
		})();
		return () => {
			stale = true;
		};
	});
</script>

<h1 class="mb-6 font-display text-3xl font-medium tracking-[0.01em] text-foreground">
	Token Usage
</h1>

<div class="mb-6 flex flex-wrap gap-4">
	<div class="grid gap-2">
		<Label for="usage-date-range">Date range</Label>
		<Popover.Root>
			<Popover.Trigger id="usage-date-range">
				{#snippet child({ props })}
					<Button
						{...props}
						variant="outline"
						class="w-fit min-w-64 justify-start rounded-sm font-normal tabular-nums"
					>
						<RiCalendarLine data-icon="inline-start" class="text-muted-foreground" />
						{#if range?.start}
							{#if range.end}
								{df.format(range.start.toDate('UTC'))} – {df.format(range.end.toDate('UTC'))}
							{:else}
								{df.format(range.start.toDate('UTC'))}
							{/if}
						{:else}
							<span class="text-muted-foreground">Pick a date range</span>
						{/if}
					</Button>
				{/snippet}
			</Popover.Trigger>
			<Popover.Content class="w-auto p-0" align="start">
				<RangeCalendar bind:value={range} numberOfMonths={2} maxDays={366} />
			</Popover.Content>
		</Popover.Root>
	</div>
</div>

<div class="space-y-6" aria-busy={loading}>
	<Card.Root>
		<Card.Header>
			<Card.Title>Tokens per day</Card.Title>
			<Card.Description>
				{#if !loaded}
					<Skeleton class="mt-1 h-4 w-72" />
				{:else}
					{n(sum((d) => d.input_tokens))} input · {n(sum((d) => d.output_tokens))} output ·
					{n(sum((d) => d.calls))} calls ({n(sum((d) => d.failed_calls))} failed) ·
					{n(sum((d) => d.cache_read_tokens))} cache read
				{/if}
			</Card.Description>
		</Card.Header>
		<Card.Content>
			{#if !loaded}
				<Skeleton class="h-64 w-full rounded-lg" />
			{:else}
				<Chart.Container config={chartConfig} class="aspect-auto h-64 w-full">
					<BarChart
						data={daily}
						x="date"
						axis="x"
						{series}
						seriesLayout="stack"
						props={{ xAxis: { format: (d: string) => d.slice(5), ticks: 8 } }}
					>
						{#snippet tooltip()}
							<Chart.Tooltip />
						{/snippet}
					</BarChart>
				</Chart.Container>
			{/if}
		</Card.Content>
	</Card.Root>

	<Card.Root>
		<Card.Header>
			<Card.Title>Breakdown</Card.Title>
		</Card.Header>
		<Card.Content class="space-y-4">
			<Tabs.Root bind:value={by}>
				<Tabs.List>
					<Tabs.Trigger value="repository">Repository</Tabs.Trigger>
					<Tabs.Trigger value="provider">Provider</Tabs.Trigger>
					<Tabs.Trigger value="key">Key</Tabs.Trigger>
					<Tabs.Trigger value="model">Model</Tabs.Trigger>
				</Tabs.List>
			</Tabs.Root>
			{#if !loaded}
				<TableSkeleton
					headers={[by, 'Input', 'Cache read', 'Output', 'Total', 'Calls']}
					right={['Input', 'Cache read', 'Output', 'Total', 'Calls']}
				/>
			{:else}
				<Table.Root>
					<Table.Header>
						<Table.Row>
							<Table.Head>{by}</Table.Head>
							<Table.Head class="text-right">Input</Table.Head>
							<Table.Head class="text-right">Cache read</Table.Head>
							<Table.Head class="text-right">Output</Table.Head>
							<Table.Head class="text-right">Total</Table.Head>
							<Table.Head class="text-right">Calls</Table.Head>
						</Table.Row>
					</Table.Header>
					<Table.Body>
						{#each rows as r (r.id ?? r.label)}
							<Table.Row>
								<Table.Cell>{r.label}</Table.Cell>
								<Table.Cell class="text-right tabular-nums">{n(r.input_tokens)}</Table.Cell>
								<Table.Cell class="text-right tabular-nums">{n(r.cache_read_tokens)}</Table.Cell>
								<Table.Cell class="text-right tabular-nums">{n(r.output_tokens)}</Table.Cell>
								<Table.Cell class="text-right tabular-nums">
									{n(r.input_tokens + r.output_tokens)}
								</Table.Cell>
								<Table.Cell class="text-right tabular-nums">{n(r.calls)}</Table.Cell>
							</Table.Row>
						{:else}
							<Table.Row>
								<Table.Cell colspan={6} class="text-muted-foreground">
									No usage in this range.
								</Table.Cell>
							</Table.Row>
						{/each}
					</Table.Body>
				</Table.Root>
			{/if}
		</Card.Content>
	</Card.Root>
</div>
