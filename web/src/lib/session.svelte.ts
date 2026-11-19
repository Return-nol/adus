import { goto } from '$app/navigation';
import { api, ApiError } from '$lib/api';

export type User = {
	id: string;
	email: string;
	role: 'admin' | 'user';
	disabled: boolean;
	github_login: string | null;
	created_at: string;
};

/** Current user. `ready` turns true once the first status/me check has finished. */
export const session = $state<{ user: User | null; ready: boolean }>({ user: null, ready: false });

/** Checks first-run setup and the login cookie; redirects to /setup or /login when needed. */
export async function loadSession(): Promise<void> {
	const status = await api<{ needs_setup: boolean }>('GET', '/auth/status');
	if (status.needs_setup) {
		session.user = null;
		session.ready = true;
		await goto('/setup');
		return;
	}
	try {
		session.user = await api<User>('GET', '/auth/me');
	} catch (e) {
		// api() already redirected to /login on 401.
		if (!(e instanceof ApiError && e.status === 401)) throw e;
		session.user = null;
	}
	session.ready = true;
}

export async function logout(): Promise<void> {
	await api<void>('POST', '/auth/logout');
	session.user = null;
	await goto('/login');
}
