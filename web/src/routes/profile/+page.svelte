<script lang="ts">
	import { toast } from 'svelte-sonner';
	import { api, ApiError } from '#lib/api.js';
	import { session, type User } from '#lib/session.svelte.js';
	import { Button } from '#lib/components/ui/button/index.js';
	import * as Card from '#lib/components/ui/card/index.js';
	import { Input } from '#lib/components/ui/input/index.js';
	import { Label } from '#lib/components/ui/label/index.js';

	let githubLogin = $state(session.user?.github_login ?? '');
	let currentPassword = $state('');
	let newPassword = $state('');

	function fail(e: unknown) {
		toast.error(e instanceof ApiError ? e.message : 'Could not reach the server');
	}

	async function saveLogin(event: SubmitEvent) {
		event.preventDefault();
		try {
			const user = await api<User>('PUT', '/me', {
				github_login: githubLogin.trim()
			});
			session.user = user;
			toast.success('GitHub login saved');
		} catch (e) {
			fail(e);
		}
	}

	async function changePassword(event: SubmitEvent) {
		event.preventDefault();
		try {
			await api<void>('PUT', '/me/password', {
				current_password: currentPassword,
				new_password: newPassword
			});
			currentPassword = newPassword = '';
			toast.success('Password changed');
		} catch (e) {
			fail(e);
		}
	}
</script>

<h1 class="mb-1 font-display text-3xl font-medium tracking-[0.01em] text-foreground">Profile</h1>
<p class="mb-6 text-sm text-muted-foreground">{session.user?.email} ({session.user?.role})</p>

<div class="grid max-w-md gap-6">
	<Card.Root>
		<Card.Header>
			<Card.Title>GitHub login</Card.Title>
			<Card.Description>
				Used to match your <code>@adudu</code> commands on GitHub. Leave empty to clear.
			</Card.Description>
		</Card.Header>
		<Card.Content>
			<form class="grid gap-4" onsubmit={saveLogin}>
				<div class="grid gap-2">
					<Label for="github">GitHub username</Label>
					<Input id="github" bind:value={githubLogin} />
				</div>
				<Button type="submit">Save</Button>
			</form>
		</Card.Content>
	</Card.Root>

	<Card.Root>
		<Card.Header>
			<Card.Title>Change password</Card.Title>
		</Card.Header>
		<Card.Content>
			<form class="grid gap-4" onsubmit={changePassword}>
				<div class="grid gap-2">
					<Label for="current">Current password</Label>
					<Input
						id="current"
						type="password"
						autocomplete="current-password"
						required
						bind:value={currentPassword}
					/>
				</div>
				<div class="grid gap-2">
					<Label for="new">New password (at least 8 characters)</Label>
					<Input
						id="new"
						type="password"
						autocomplete="new-password"
						minlength={8}
						required
						bind:value={newPassword}
					/>
				</div>
				<Button type="submit">Change password</Button>
			</form>
		</Card.Content>
	</Card.Root>
</div>
