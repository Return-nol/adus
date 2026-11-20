<script lang="ts">
	import { resetMode, setMode, userPrefersMode } from 'mode-watcher';
	import RiComputerLine from 'remixicon-svelte/icons/computer-line';
	import RiMoonLine from 'remixicon-svelte/icons/moon-line';
	import RiSunLine from 'remixicon-svelte/icons/sun-line';
	import { cn } from '#lib/utils.js';

	let { compact = false, class: className }: { compact?: boolean; class?: string } = $props();

	const options = [
		{
			value: 'system',
			label: 'Auto',
			title: 'System theme',
			icon: RiComputerLine,
			select: () => resetMode()
		},
		{
			value: 'light',
			label: 'Light',
			title: 'Light theme',
			icon: RiSunLine,
			select: () => setMode('light')
		},
		{
			value: 'dark',
			label: 'Dark',
			title: 'Dark theme',
			icon: RiMoonLine,
			select: () => setMode('dark')
		}
	] as const;
</script>

<div
	role="radiogroup"
	aria-label="Theme"
	class={cn(
		'inline-grid grid-cols-3 gap-0.5 rounded-sm border border-border/60 bg-muted p-0.5',
		!compact && 'w-full',
		className
	)}
>
	{#each options as opt (opt.value)}
		{@const active = userPrefersMode.current === opt.value}
		<button
			type="button"
			role="radio"
			aria-checked={active}
			aria-label={opt.title}
			title={opt.title}
			onclick={opt.select}
			class={cn(
				'inline-flex items-center justify-center gap-1.5 rounded-[4px] text-xs font-medium transition-colors outline-none focus-visible:ring-2 focus-visible:ring-ring/50',
				compact ? 'size-7' : 'px-2 py-1.5',
				active
					? 'bg-background text-foreground shadow-xs ring-1 ring-border/70'
					: 'text-muted-foreground hover:text-foreground'
			)}
		>
			<opt.icon class="size-3.5 shrink-0" />
			{#if !compact}<span>{opt.label}</span>{/if}
		</button>
	{/each}
</div>
