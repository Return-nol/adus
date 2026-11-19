<script lang="ts">
	import { toast } from 'svelte-sonner';
	import { api, ApiError } from '#lib/api.js';
	import type { User } from '#lib/session.svelte.js';
	import { Button } from '#lib/components/ui/button/index.js';
	import * as Dialog from '#lib/components/ui/dialog/index.js';
	import { Input } from '#lib/components/ui/input/index.js';
	import { Label } from '#lib/components/ui/label/index.js';
	import * as Select from '#lib/components/ui/select/index.js';

	// `user` set = edit that user; null = create a new one. `open` is bindable.
	let {
		open = $bindable(false),
		user,
		onsaved
	}: { open: boolean; user: User | null; onsaved: () => void } = $props();

	let email = $state('');
	let password = $state('');
	let role = $state<'admin' | 'user'>('user');
	let githubLogin = $state('');
	let busy = $state(false);

	// Reset the form each time the dialog opens.
	$effect(() => {
		if (!open) return;
		email = user?.email ?? '';
		password = '';
		role = user?.role ?? 'user';
		githubLogin = user?.github_login ?? '';
	});

	async function submit(event: SubmitEvent) {
		event.preventDefault();
		busy = true;
		try {
			if (user) {
				// An empty string clears the GitHub login.
				await api('PATCH', `/users/${user.id}`, { role, github_login: githubLogin.trim() });
			} else {
				await api('POST', '/users', { email, password, role, github_login: githubLogin.trim() });
			}
			open = false;
			toast.success(user ? 'User updated' : 'User created');
			onsaved();
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
			<Dialog.Title>{user ? `Edit ${user.email}` : 'Add user'}</Dialog.Title>
		</Dialog.Header>
		<form class="grid gap-4" onsubmit={submit}>
			{#if !user}
				<div class="grid gap-2">
					<Label for="u-email">Email</Label>
					<Input id="u-email" type="email" required bind:value={email} />
				</div>
				<div class="grid gap-2">
					<Label for="u-password">Password (at least 8 characters)</Label>
					<Input id="u-password" type="password" minlength={8} required bind:value={password} />
				</div>
			{/if}
			<div class="grid gap-2">
				<Label>Role</Label>
				<Select.Root type="single" bind:value={role}>
					<Select.Trigger class="w-full">{role === 'admin' ? 'Admin' : 'User'}</Select.Trigger>
					<Select.Content>
						<Select.Item value="user" label="User">User</Select.Item>
						<Select.Item value="admin" label="Admin">Admin</Select.Item>
					</Select.Content>
				</Select.Root>
			</div>
			<div class="grid gap-2">
				<Label for="u-github">GitHub login (optional)</Label>
				<Input id="u-github" bind:value={githubLogin} />
			</div>
			<Dialog.Footer>
				<Button type="submit" disabled={busy}>{user ? 'Save' : 'Create'}</Button>
			</Dialog.Footer>
		</form>
	</Dialog.Content>
</Dialog.Root>
