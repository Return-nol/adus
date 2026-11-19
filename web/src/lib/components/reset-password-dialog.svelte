<script lang="ts">
	import { toast } from 'svelte-sonner';
	import { api, ApiError } from '$lib/api';
	import type { User } from '$lib/session.svelte';
	import { Button } from '$lib/components/ui/button/index.js';
	import * as Dialog from '$lib/components/ui/dialog/index.js';
	import { Input } from '$lib/components/ui/input/index.js';
	import { Label } from '$lib/components/ui/label/index.js';

	let { open = $bindable(false), user }: { open: boolean; user: User | null } = $props();

	let password = $state('');
	let busy = $state(false);

	$effect(() => {
		if (open) password = '';
	});

	async function submit(event: SubmitEvent) {
		event.preventDefault();
		if (!user) return;
		busy = true;
		try {
			await api('POST', `/users/${user.id}/password`, { password });
			open = false;
			toast.success('Password reset');
		} catch (e) {
			toast.error(e instanceof ApiError ? e.message : 'Could not reach the server');
		} finally {
			busy = false;
		}
	}
</script>

<Dialog.Root bind:open>
	<Dialog.Content>
		<Dialog.Header>
			<Dialog.Title>Reset password for {user?.email}</Dialog.Title>
			<Dialog.Description>Their other sessions are signed out.</Dialog.Description>
		</Dialog.Header>
		<form class="grid gap-4" onsubmit={submit}>
			<div class="grid gap-2">
				<Label for="r-password">New password (at least 8 characters)</Label>
				<Input id="r-password" type="password" minlength={8} required bind:value={password} />
			</div>
			<Dialog.Footer>
				<Button type="submit" disabled={busy}>Reset password</Button>
			</Dialog.Footer>
		</form>
	</Dialog.Content>
</Dialog.Root>
