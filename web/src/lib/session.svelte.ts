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

/**
 * Current user. `ready` turns true once the first status/me check has finished;
 * `error` is true when that check failed for a reason other than "not logged in".
 */
export const session = $state<{ user: User | null; ready: boolean; error: boolean }>({
	user: null,
	ready: false,
	error: false
});

// Bumped by every session change so a slower, older `loadSession` cannot overwrite a newer result.
let generation = 0;

/** Call after a successful login or setup. Also invalidates any `loadSession` still in flight. */
export function setUser(user: User): void {
	generation++;
	session.user = user;
	session.ready = true;
	session.error = false;
}

/** Checks first-run setup and the login cookie; redirects to /setup or /login when needed. */
export async function loadSession(): Promise<void> {
	const mine = ++generation;
	session.ready = false;
	session.error = false;
	try {
		const status = await api<{ needs_setup: boolean }>('GET', '/auth/status');
		if (mine !== generation) return;
		if (status.needs_setup) {
			session.user = null;
			session.ready = true;
			await goto('/setup');
			return;
		}
		// No automatic /login redirect here: we decide below, once we know the result is still current.
		const user = await api<User>('GET', '/auth/me', undefined, { redirectOn401: false });
		if (mine !== generation) return;
		session.user = user;
	} catch (e) {
		if (mine !== generation) return;
		if (e instanceof ApiError && e.status === 401) {
			session.user = null;
			// Public pages (/login, /setup) stay where they are.
			if (!['/login', '/setup'].includes(location.pathname)) await goto('/login');
		} else {
			session.error = true;
		}
	}
	session.ready = true;
}

export async function logout(): Promise<void> {
	await api<void>('POST', '/auth/logout');
	generation++;
	session.user = null;
	await goto('/login');
}
