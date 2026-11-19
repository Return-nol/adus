<script lang="ts" module>
	export type Kind = 'azure_devops' | 'github';
	export type WebhookError = { repository_id: string; message: string };
	export type Connection = {
		id: string;
		kind: Kind;
		name: string;
		org_or_owner: string;
		token_last4: string;
		status: 'ok' | 'invalid' | 'unverified';
		pat_owner: string | null;
		last_verified_at: string | null;
		repository_count: number;
	};
</script>

<script lang="ts">
	import { toast } from 'svelte-sonner';
	import { api, ApiError } from '#lib/api.js';
	import { Button } from '#lib/components/ui/button/index.js';
	import * as Dialog from '#lib/components/ui/dialog/index.js';
	import { Input } from '#lib/components/ui/input/index.js';
	import { Label } from '#lib/components/ui/label/index.js';
	import * as Select from '#lib/components/ui/select/index.js';

	// `connection` set = rotate that connection's PAT; null = create a new connection.
	let {
		open = $bindable(false),
		connection,
		onsaved
	}: {
		open: boolean;
		connection: Connection | null;
		onsaved: (webhookErrors: WebhookError[]) => void;
	} = $props();

	let kind = $state<Kind>('github');
	let name = $state('');
	let orgOrOwner = $state('');
	let token = $state('');
	let error = $state('');
	let busy = $state(false);

	// Reset the form each time the dialog opens.
	$effect(() => {
		if (!open) return;
		kind = 'github';
		name = '';
		orgOrOwner = '';
		token = '';
		error = '';
	});

	const kindLabel = { azure_devops: 'Azure DevOps', github: 'GitHub' };
	const shownKind = $derived(connection?.kind ?? kind);

	async function submit(event: SubmitEvent) {
		event.preventDefault();
		busy = true;
		error = '';
		try {
			let webhookErrors: WebhookError[] = [];
			if (connection) {
				const reply = await api<{ webhook_errors: WebhookError[] }>(
					'PATCH',
					`/connections/${connection.id}`,
					{ token }
				);
				webhookErrors = reply.webhook_errors;
			} else {
				await api('POST', '/connections', {
					kind,
					name,
					org_or_owner: orgOrOwner,
					token
				});
			}
			token = '';
			open = false;
			toast.success(connection ? 'PAT rotated' : 'Connection created');
			onsaved(webhookErrors);
		} catch (e) {
			// 422 token_invalid and friends: show inline and keep the dialog open.
			error = e instanceof ApiError ? e.message : 'Could not reach the server';
		} finally {
			busy = false;
		}
	}
</script>

<Dialog.Root bind:open>
	<Dialog.Content>
		<Dialog.Header>
			<Dialog.Title>
				{connection ? `Rotate PAT for ${connection.name}` : 'Add connection'}
			</Dialog.Title>
			{#if connection}
				<Dialog.Description>
					The new token is verified first. Repositories stay and missing webhooks are recreated.
				</Dialog.Description>
			{/if}
		</Dialog.Header>
		<form class="grid gap-4" onsubmit={submit}>
			{#if !connection}
				<div class="grid gap-2">
					<Label>Provider</Label>
					<Select.Root type="single" bind:value={kind}>
						<Select.Trigger class="w-full">{kindLabel[kind]}</Select.Trigger>
						<Select.Content>
							<Select.Item value="github" label="GitHub">GitHub</Select.Item>
							<Select.Item value="azure_devops" label="Azure DevOps">Azure DevOps</Select.Item>
						</Select.Content>
					</Select.Root>
				</div>
				<div class="grid gap-2">
					<Label for="c-name">Name</Label>
					<Input id="c-name" required bind:value={name} />
				</div>
				<div class="grid gap-2">
					<Label for="c-org">
						{kind === 'azure_devops' ? 'Organization' : 'Owner (user or organization)'}
					</Label>
					<Input id="c-org" required bind:value={orgOrOwner} />
				</div>
			{/if}
			<div class="grid gap-2">
				<Label for="c-token">Personal access token</Label>
				<Input id="c-token" type="password" autocomplete="off" required bind:value={token} />
				<p class="text-xs text-muted-foreground">
					{#if shownKind === 'github'}
						Minimum scopes. Fine-grained: Pull requests (read and write), Contents (read), Webhooks
						(read and write), Metadata (read). Classic: <code>repo</code>,
						<code>admin:repo_hook</code>.
					{:else}
						Minimum scopes: Code (read and write), Service Hooks (read, write and manage).
					{/if}
				</p>
			</div>
			{#if error}<p class="text-sm text-destructive" role="alert">{error}</p>{/if}
			<Dialog.Footer>
				<Button type="submit" disabled={busy}>{connection ? 'Rotate PAT' : 'Create'}</Button>
			</Dialog.Footer>
		</form>
	</Dialog.Content>
</Dialog.Root>
