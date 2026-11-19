<script lang="ts">
	import { onMount } from 'svelte';
	import { goto } from '$app/navigation';
	import { api, ApiError } from '$lib/api';
	import { session, type User } from '$lib/session.svelte';
	import { Button } from '$lib/components/ui/button/index.js';
	import * as Card from '$lib/components/ui/card/index.js';
	import { Input } from '$lib/components/ui/input/index.js';
	import { Label } from '$lib/components/ui/label/index.js';

	let email = $state('');
	let password = $state('');
	let error = $state('');
	let busy = $state(false);

	// Setup only exists until the first admin is created.
	onMount(async () => {
		const status = await api<{ needs_setup: boolean }>('GET', '/auth/status');
		if (!status.needs_setup) await goto('/login', { replaceState: true });
	});

	async function submit(event: SubmitEvent) {
		event.preventDefault();
		busy = true;
		error = '';
		try {
			// Setup also starts a session, so the new admin is logged in.
			session.user = await api<User>('POST', '/auth/setup', { email, password });
			await goto('/repositories');
		} catch (e) {
			error = e instanceof ApiError ? e.message : 'Could not reach the server';
		} finally {
			busy = false;
		}
	}
</script>

<div class="flex min-h-screen items-center justify-center p-4">
	<Card.Root class="w-full max-w-sm">
		<Card.Header>
			<Card.Title>Welcome to Adus</Card.Title>
			<Card.Description>Create the first admin account.</Card.Description>
		</Card.Header>
		<Card.Content>
			<form class="grid gap-4" onsubmit={submit}>
				<div class="grid gap-2">
					<Label for="email">Email</Label>
					<Input id="email" type="email" autocomplete="username" required bind:value={email} />
				</div>
				<div class="grid gap-2">
					<Label for="password">Password (at least 8 characters)</Label>
					<Input
						id="password"
						type="password"
						autocomplete="new-password"
						minlength={8}
						required
						bind:value={password}
					/>
				</div>
				{#if error}<p class="text-sm text-destructive" role="alert">{error}</p>{/if}
				<Button type="submit" disabled={busy}>Create admin</Button>
			</form>
		</Card.Content>
	</Card.Root>
</div>
