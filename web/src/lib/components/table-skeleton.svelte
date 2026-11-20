<script lang="ts">
	import * as Card from '#lib/components/ui/card/index.js';
	import { Skeleton } from '#lib/components/ui/skeleton/index.js';
	import * as Table from '#lib/components/ui/table/index.js';

	// Placeholder table shown while a page loads: real headers, pulsing cells.
	// `right` lists the headers whose column is right-aligned (like the real table).
	let {
		headers,
		right = ['Actions'],
		rows = 5,
		card = false
	}: { headers: string[]; right?: string[]; rows?: number; card?: boolean } = $props();

	// Varied widths so the rows don't look like a grid of identical bars.
	const widths = ['w-40', 'w-24', 'w-32', 'w-20', 'w-28'];
</script>

{#snippet table()}
	<Table.Root aria-busy="true">
		<Table.Header>
			<Table.Row>
				{#each headers as h (h)}
					<Table.Head class={right.includes(h) ? 'text-right' : ''}>{h}</Table.Head>
				{/each}
			</Table.Row>
		</Table.Header>
		<Table.Body>
			{#each { length: rows } as _, row (row)}
				<Table.Row class="hover:bg-transparent">
					{#each headers as h, col (h)}
						<Table.Cell class="py-4">
							<Skeleton
								class={`h-4 ${right.includes(h) ? 'ml-auto w-16' : widths[(row + col) % widths.length]}`}
							/>
						</Table.Cell>
					{/each}
				</Table.Row>
			{/each}
		</Table.Body>
	</Table.Root>
	<span class="sr-only" role="status">Loading…</span>
{/snippet}

{#if card}
	<Card.Root class="py-0">
		<Card.Content class="px-0">{@render table()}</Card.Content>
	</Card.Root>
{:else}
	{@render table()}
{/if}
