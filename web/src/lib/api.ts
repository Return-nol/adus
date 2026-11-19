import { goto } from '$app/navigation';

export class ApiError extends Error {
	status: number;
	code: string;

	constructor(status: number, code: string, message: string) {
		super(message);
		this.status = status;
		this.code = code;
	}
}

type Method = 'GET' | 'POST' | 'PUT' | 'PATCH' | 'DELETE';

/** Calls `/api/v1{path}` with the session cookie. Throws `ApiError`; a 401 also redirects to /login. */
export async function api<T>(method: Method, path: string, body?: unknown): Promise<T> {
	const res = await fetch(`/api/v1${path}`, {
		method,
		credentials: 'include',
		headers: body === undefined ? undefined : { 'Content-Type': 'application/json' },
		body: body === undefined ? undefined : JSON.stringify(body)
	});

	if (!res.ok) {
		// Error body is `{"error":{"code","message"}}`; fall back for anything else (e.g. a proxy error page).
		const payload = await res.json().catch(() => null);
		const err = new ApiError(
			res.status,
			payload?.error?.code ?? 'unknown',
			payload?.error?.message ?? res.statusText
		);
		if (res.status === 401) await goto('/login');
		throw err;
	}

	if (res.status === 204) return undefined as T;
	return (await res.json()) as T;
}
