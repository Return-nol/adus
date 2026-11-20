<script lang="ts">
	import { onMount } from 'svelte';
	import { toast } from 'svelte-sonner';
	import { api, ApiError } from '#lib/api.js';
	import { session, type User } from '#lib/session.svelte.js';
	import ResetPasswordDialog from '#lib/components/reset-password-dialog.svelte';
	import UserFormDialog from '#lib/components/user-form-dialog.svelte';
	import { Badge } from '#lib/components/ui/badge/index.js';
	import { Button } from '#lib/components/ui/button/index.js';
	import * as Card from '#lib/components/ui/card/index.js';
	import { Switch } from '#lib/components/ui/switch/index.js';
	import * as Table from '#lib/components/ui/table/index.js';

	let users = $state<User[]>([]);
	let forbidden = $state(false);
	let formOpen = $state(false);
	let resetOpen = $state(false);
	let selected = $state<User | null>(null);

	async function load() {
		try {
			users = await api<User[]>('GET', '/users');
		} catch (e) {
			if (e instanceof ApiError && e.status === 403) forbidden = true;
			else toast.error(e instanceof ApiError ? e.message : 'Could not reach the server');
		}
	}

	onMount(load);

	function openForm(user: User | null) {
		selected = user;
		formOpen = true;
	}

	function openReset(user: User) {
		selected = user;
		resetOpen = true;
	}

	async function setActive(user: User, active: boolean) {
		try {
			await api('PATCH', `/users/${user.id}`, { disabled: !active });
			await load();
		} catch (e) {
			// e.g. 409 last_admin; the switch snaps back because it reads `user.disabled`.
			toast.error(e instanceof ApiError ? e.message : 'Could not reach the server');
		}
	}
</script>

<div class="mb-6 flex items-center justify-between">
	<h1 class="font-display text-3xl font-medium tracking-[0.01em] text-foreground">Users</h1>
	{#if !forbidden}<Button onclick={() => openForm(null)}>Add user</Button>{/if}
</div>

{#if forbidden}
	<div
		class="rounded-xl border border-dashed border-border bg-card px-8 py-12 text-center text-sm text-muted-foreground"
	>
		Only admins can manage users.
	</div>
{:else}
	<Card.Root>
		<Card.Content>
			<Table.Root>
				<Table.Header>
					<Table.Row>
						<Table.Head>Email</Table.Head>
						<Table.Head>Role</Table.Head>
						<Table.Head>GitHub login</Table.Head>
						<Table.Head>Active</Table.Head>
						<Table.Head class="text-right">Actions</Table.Head>
					</Table.Row>
				</Table.Header>
				<Table.Body>
					{#each users as user (user.id)}
						<Table.Row>
							<Table.Cell>{user.email}</Table.Cell>
							<Table.Cell>
								<Badge variant={user.role === 'admin' ? 'default' : 'secondary'}>{user.role}</Badge>
							</Table.Cell>
							<Table.Cell>{user.github_login ?? ''}</Table.Cell>
							<Table.Cell>
								<Switch
									aria-label={`Active: ${user.email}`}
									disabled={user.id === session.user?.id}
									bind:checked={() => !user.disabled, (active) => setActive(user, active)}
								/>
							</Table.Cell>
							<Table.Cell class="space-x-2 text-right">
								<Button variant="outline" size="sm" onclick={() => openForm(user)}>Edit</Button>
								<Button variant="outline" size="sm" onclick={() => openReset(user)}>
									Reset password
								</Button>
							</Table.Cell>
						</Table.Row>
					{/each}
				</Table.Body>
			</Table.Root>
		</Card.Content>
	</Card.Root>
{/if}

<UserFormDialog bind:open={formOpen} user={selected} onsaved={load} />
<ResetPasswordDialog bind:open={resetOpen} user={selected} />
