# AGENTS.md — Web Dashboard (`web/`)

Binding visual spec: [DESIGN.md](../docs/DESIGN.md) · Functional spec: [docs/SPEC.md](../docs/SPEC.md)

## SvelteKit & API Conventions

- **Stack:** Svelte 5 runes and TypeScript with no state library. UI copy is in English.
- **Imports (`#lib`, never `$lib`):** SvelteKit 3 replaced `$lib` with `package.json` subpath imports (`#lib` / `#lib/*`). Always import from `#lib/...` and include extensions: `.js` for `.ts` modules (e.g. `#lib/api.js`, `#lib/session.svelte.js`, `#lib/utils.js`) and `.svelte` for components. Never re-add `alias` to `vite.config.ts` (triggers `config_option_deprecated_alias` and `tsconfig_invalid` warnings). `web/components.json` configures `shadcn-svelte` under `#lib`.
- **Auth & API calls:**
  - Session store: `web/src/lib/session.svelte.ts` (stale `/auth/me` results are ignored).
  - All HTTP calls go through `web/src/lib/api.ts` (prefix `/api/v1`, `credentials: 'include'`, throws `ApiError`, redirects to `/login` on 401 unless `{ redirectOn401: false }`).
- **UI patterns & RBAC:**
  - Dialogs follow `web/src/lib/components/*-dialog.svelte`.
  - Destructive actions use an alert-dialog; notices use Sonner toasts (do not re-enable `richColors`).
  - Role `user` sees pages read-only with admin controls hidden or disabled via `session.user.role` (except Rules/Memory management and Profile).
  - Never reconstruct repository `display_name` in the frontend; always display the server-provided `display_name`.

## Visual Design & Theming (`DESIGN.md`)

- All work in `web/` must follow [DESIGN.md](../docs/DESIGN.md) for both light and dark themes.
- Theme tokens live in `web/src/routes/layout.css` as OKLCH custom properties (warm cream light mode `#fffefb`/`#f8f4f0`/`#201515`, warm espresso dark mode `#201515`/`#2a2320`/`#fffefb`, single `#ff4f00` CTA accent — never pure white, pure black, cool greys, or a second chromatic accent).
- Typography uses self-hosted `@fontsource-variable/mona-sans` (`font-sans`, `font-display`, sentence-case `font-display font-medium` for `<h1>`, uppercase tracked table headers).
- Shapes use `12px` (`rounded-lg`/`rounded-xl`) for buttons/cards/modals and `6px` (`rounded-sm`) for inputs and sidebar rows.
- Light/dark/system mode is managed by `mode-watcher` (`ModeWatcher` in `web/src/routes/+layout.svelte`, `ThemeToggle` in `web/src/lib/components/theme-toggle.svelte`, pre-paint no-FOUC script in `web/src/app.html`).
- Reuse semantic tokens (`bg-background`, `bg-card`, `text-foreground`, `text-muted-foreground`, `border-border`, `bg-primary`) and aligned `shadcn-svelte` primitives. Style empty and admin-only states with the dashed card container.

## Loading States (Skeletons)

Every data-fetching page shows a skeleton until its first load finishes — never a blank area or premature empty state:

1. **Flag:** Keep `loaded = $state(false)` and set it `true` in the load function even on failure. Pages with a retry card (Review Rules, Reviews, Settings) also keep `loadError`. System Status treats `report === null` as loading.
2. **Branch order:** `{#if loadError}` → `{:else if !loaded}` skeleton → `{:else if items.length === 0}` dashed empty state → real content.
3. **Reloads:** Pages scoped by a filter reset `loaded = false` when the filter changes (Review Rules, Reviews). Background polling and post-mutation reloads keep current rows. Token Usage shows the skeleton only on first load.
4. **Buttons:** Primary header actions ("Add …") use `disabled={!loaded}`. Row actions render only after data loads.
5. **Tables:** Use `#lib/components/table-skeleton.svelte` (`headers` = real column labels, `right` = right-aligned columns, `card` = matches `Card` wrapper).
6. **Other layouts:** Cards, forms, and charts use `Skeleton` from `#lib/components/ui/skeleton` matching the real card/header/badge/button geometry.
7. **Sync & new pages:** Updating a page's layout (columns, headers, card actions, form fields, nesting) must update its skeleton in the same change. Every new data page ships with a skeleton.
8. **Manual check:** Temporarily make `api()` in `web/src/lib/api.ts` return `new Promise(() => {})` for GETs outside `/auth/` (never commit).

## Verification

- Run `pnpm -C web check` (must report 0 errors) and `pnpm -C web build` before Rust SPA tests or release builds.
- Do not run headless browser automation against the dashboard (see root [AGENTS.md](../AGENTS.md)); verify UI changes via code inspection, `pnpm -C web check`, and curl against `/api/v1/*`.
