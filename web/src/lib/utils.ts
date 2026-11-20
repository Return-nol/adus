export { cn } from 'cn';

// eslint-disable-next-line @typescript-eslint/no-explicit-any
export type WithoutChild<T> = T extends { child?: any } ? Omit<T, 'child'> : T;
// eslint-disable-next-line @typescript-eslint/no-explicit-any
export type WithoutChildren<T> = T extends { children?: any } ? Omit<T, 'children'> : T;
export type WithoutChildrenOrChild<T> = WithoutChildren<WithoutChild<T>>;
export type WithElementRef<T, U extends HTMLElement = HTMLElement> = T & { ref?: U | null };

/** Returns `u` only if it parses as an http(s) URL, so it is safe in `<a href>`; otherwise null. */
export function safeHttpUrl(u: string | null): string | null {
	if (!u) return null;
	try {
		const { protocol } = new URL(u);
		return protocol === 'http:' || protocol === 'https:' ? u : null;
	} catch {
		return null;
	}
}
