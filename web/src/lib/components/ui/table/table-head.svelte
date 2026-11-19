<script lang="ts">
	import { cn, type WithElementRef } from '#lib/utils.js';
	import type { HTMLThAttributes } from 'svelte/elements';

	let {
		ref = $bindable(null),
		class: className,
		children,
		...restProps
	}: WithElementRef<HTMLThAttributes> = $props();
</script>

<th
	bind:this={ref}
	data-slot="table-head"
	class={cn(
		'h-10 px-3 text-left align-middle font-display text-xs font-medium tracking-[0.08em] whitespace-nowrap text-muted-foreground uppercase [&:has([role=checkbox])]:pr-0',
		className
	)}
	{...restProps}
>
	{@render children?.()}
</th>
