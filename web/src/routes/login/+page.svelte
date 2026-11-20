<script lang="ts">
	import { goto } from '$app/navigation';
	import { api, ApiError } from '#lib/api.js';
	import { session, setUser, type User } from '#lib/session.svelte.js';
	import { Button } from '#lib/components/ui/button/index.js';
	import * as Card from '#lib/components/ui/card/index.js';
	import { Input } from '#lib/components/ui/input/index.js';
	import { Label } from '#lib/components/ui/label/index.js';

	let email = $state('');
	let password = $state('');
	let error = $state('');
	let busy = $state(false);

	// Already logged in (or just logged in): go to the app.
	$effect(() => {
		if (session.user) goto('/repositories', { replaceState: true });
	});

	async function submit(event: SubmitEvent) {
		event.preventDefault();
		busy = true;
		error = '';
		try {
			setUser(await api<User>('POST', '/auth/login', { email, password }));
		} catch (e) {
			error = e instanceof ApiError ? e.message : 'Could not reach the server';
		} finally {
			busy = false;
		}
	}
</script>

<div class="flex min-h-screen items-center justify-center p-4">
	<Card.Root class="w-full max-w-md">
		<Card.Header>
			<p class="font-display text-xs font-medium tracking-[0.12em] text-primary uppercase">Adus 🛀</p>
			<Card.Title class="font-display text-2xl font-medium tracking-tight">
				Dashboard Log in
			</Card.Title>
		</Card.Header>
		<Card.Content>
			<form class="grid gap-4" onsubmit={submit}>
				<div class="grid gap-2">
					<Label for="email">Email</Label>
					<Input
						id="email"
						name="email"
						type="email"
						autocomplete="username"
						required
						bind:value={email}
					/>
				</div>
				<div class="grid gap-2">
					<Label for="password">Password</Label>
					<Input
						id="password"
						name="password"
						type="password"
						autocomplete="current-password"
						required
						bind:value={password}
					/>
				</div>
				{#if error}<p class="text-sm text-destructive" role="alert">{error}</p>{/if}
				<Button type="submit" class="w-full" disabled={busy}>Log in</Button>
			</form>
		</Card.Content>
	</Card.Root>
</div>
