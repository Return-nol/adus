# AGENTS.md — Adus

Adus is a self-hosted, open-source AI code review platform. Its bot is **`@adudu`**. It reviews pull requests on **Azure DevOps Services** (`dev.azure.com`) and **GitHub**, both connected with a PAT. The backend is Rust and the dashboard is SvelteKit with shadcn-svelte. Adus is inspired by [Kodus](https://github.com/kodustech/kodus-ai), but it is a rewrite, not a port. Licensed AGPL-3.0.

Repo: https://github.com/Return-nol/adus. The GHCR image name is `ghcr.io/return-nol/adus` (lowercase).

## Read these first

- **Design (binding authority):** `docs/superpowers/specs/2026-11-19-adus-design.md`
- **Web visual design (binding for `web/`):** `DESIGN.md` — warm cream/coffee palette, `#ff4f00` orange CTA accent, shapes, and component chrome (applied across light and dark themes).
- **Implementation plan v1 (27 tasks, 6 milestones, complete):** `docs/superpowers/plans/2026-11-19-adus-v1.md`
- **Follow-up design + plan (in progress):** `docs/superpowers/specs/2026-11-20-ignored-files-and-azure-project-design.md` and `docs/superpowers/plans/2026-11-20-ignored-files-and-azure-project.md` (7 tasks)
- **Agentic review engine (in progress on `feat/advanced-agentic-review-flow`):** `docs/superpowers/specs/2026-11-21-agentic-review-engine-design.md` and `docs/superpowers/plans/2026-11-21-agentic-review-engine.md` (10 tasks).
- **Backlog of deferred minor findings:** `docs/superpowers/backlog.md`. (The local SDD ledgers used during execution were deleted after both plans finished; git history is the record.)

If the spec and the plan conflict, the spec wins. For new work, start with superpowers `brainstorming`, then `writing-plans`, then `subagent-driven-development`. Every plan and subagent prompt must carry the **Clean-room and no-assumption rule** (section below).

## Status (as of 2026-11-20)

**v1 plan: all 27 tasks done.** The final whole-branch review found 1 Critical and 4 Important issues; all were fixed (`7533f1b`..`c819e33`) and re-reviewed clean.

**Follow-up plan (ignored files + Azure project names), 7 tasks:**

| Task | What | State |
|---|---|---|
| 1 | Azure `project_name` (migration 0006), `display_name` from the server (`repositories::label` is the single Rust rule, `display_name_sql` its SQL twin), backfill on remote-repos and resync | done (`a0017db`, fix `a29632f`) |
| 2 | Dashboard uses `display_name` everywhere; add-repo dialog gets search, counter and sort | done (`c096657`) |
| 3 | Global ignore list (Kodus defaults) | done (`e53a4f3`) |
| 4 | Settings API for ignore paths + title keywords, test-a-file | done (`59ce5c2`) |
| 5 | Engine applies ignore + title keywords | done (`1b2e881`) |
| 6 | Settings UI for ignored files and title keywords | done (`45caa70`) |
| 7 | v1 follow-ups: no-model on `updated` → `skipped`, periodic `fail_stale`, `ChatError::Internal` text | done (`60b8b24`) |

Final review fix wave done (`6f74044`..`f8248f1`): doctor webhook label uses `display_name`, ignore patterns drop a leading `/` and the whole list is compiled on save (`invalid_glob` 422 if too complex), `STALE_AFTER` bound into `fail_stale`, GitHub `edited` with a title change re-triggers a review, installers build the `adus` image when it cannot be pulled (`--build` / `-Build`), README upgrading note.

**Follow-up plan complete.** Its final review was fixed in `6f74044`..`75c2bfa` (incl. the installer `--build` / `-Build` flag) and re-reviewed clean. Nothing is pushed yet.

### v1 task table (historical)

Work is on branch `feat/adus-v1`. Nothing has been pushed.

| Task | What | State |
|---|---|---|
| 1 | Workspace, config, `/healthz`, dev compose | done |
| 2 | AES-256-GCM secret cipher (`adus_core::crypto`) | done |
| 3 | DB users and sessions + migration 0001 | done |
| 4 | Session auth: first-admin setup, login, logout, me | done |
| 5 | User management API | done |
| 6 | Dashboard scaffold (SvelteKit SPA embedded via rust-embed), login, setup, users, profile pages | done (click-through UI not yet browser-tested) |
| 7 | `crates/git`: `GitProvider` trait, types, diff utilities (`commentable_lines`, `added_line_count`, `unified_patch`) | done |
| 8 | GitHub PAT provider (`GithubProvider`) | done |
| 9 | Azure DevOps Services PAT provider (`AzureProvider`) + `HttpGitProviderFactory` | done |
| 10 | Connections & repositories: DB (migration 0002) + API, PAT rotation that keeps repos | done |
| 11 | Repositories page (connections, rotate PAT, resync webhooks, add/remove repos) | done (click-through not browser-tested) |
| 12 | LLM clients: OpenAI, Anthropic, Gemini, OpenAI-compatible (`crates/llm`) | done |
| 13 | AI provider/key DB (migration 0003), multi-key router with cooldown, usage logging | done |
| 14 | AI provider, key, settings and usage APIs (incl. test-key) | done |
| 15 | AI Providers, Settings, Token Usage pages | done (click-through and chart look not browser-tested) |
| 16–17 | Rules & memory (migration 0004) API + page | done |
| 18–24 | Redis Streams queue, webhooks, prompt/parser, review engine (migration 0005), `@adudu` commands, worker, Reviews page | done (Opus high) |
| 25–27 | CLI (`gen-secret`, `check-config`, `doctor`, `healthcheck`) + System status page, Docker/compose/installers, CI + GHCR release | done |

Implementers ran on Sonnet high (Opus high for the queue/webhook/engine/worker tasks), with a reviewer per task and a whole-branch review at the end.

## Layout

```
crates/core     config, crypto (Cipher, last4), shared types (ConnectionKind, ProviderKind, Severity, normalize_path)
crates/db       sqlx Postgres: connect() + migrations, users.rs, sessions.rs, connections.rs, repositories.rs, ai.rs, settings.rs, usage.rs
crates/git      GitProvider trait + types.rs, diff.rs, github.rs, azure.rs, HttpGitProviderFactory (lib.rs)
crates/llm      LlmClient per provider (openai.rs, anthropic.rs, gemini.rs, shared send in types.rs), client_for(), router.rs (LlmRouter: ChatService)
crates/queue    (Task 18+) Redis Streams job queue
crates/review   (Task 20+) prompt/parse/engine/commands
crates/server   axum: router(), state.rs, error.rs, auth.rs, spa.rs, api/*
crates/adus     binary (main.rs)
migrations/     sqlx migrations, embedded by crates/db
web/            SvelteKit SPA (adapter-static) + shadcn-svelte; build output web/build is embedded into the binary
```

Crate dependencies must stay acyclic. `core` sits under `db`, `git` and `queue`. `llm` uses `core` and `db`. `review` uses `core`, `db`, `git`, `llm` and `queue`. `server` and `adus` use everything.

## Commands

```sh
docker compose -f docker-compose.dev.yml up -d     # Postgres → localhost:55432, Redis → localhost:55379
cargo test --workspace                             # env comes from .cargo/config.toml
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
pnpm -C web install
pnpm -C web check                                  # must report 0 errors
pnpm -C web build                                  # required before the spa tests and before release builds
pnpm -C web dev                                    # http://localhost:5173, proxies /api and /webhooks to :8080
ADUS_PUBLIC_URL=http://localhost:8080 cargo run -p adus   # http://localhost:8080; dev env comes from .cargo/config.toml
```

CI: GitHub Actions. `.github/workflows/ci.yml` runs on every push and PR (job `rust`: fmt, clippy, tests against Postgres/Redis services; job `web`: check, fmt:check). `.github/workflows/release.yml` runs on `v*` tags and pushes a multi-arch (amd64 + arm64) image to `ghcr.io/return-nol/adus` tagged `{version}` and `latest`.

On Windows, if `cargo` is not on PATH, it is in `%USERPROFILE%\.cargo\bin`. The rustup MSVC toolchain is used.

## Environment gotchas

- **Dev ports.** The host already runs native Postgres on 5432 and Redis on 6379, so the dev compose maps to **55432** and **55379**. Port 56379 cannot be used because it is inside a Windows reserved TCP range. Do not stop the user's native services.
- **Docker Desktop.** It sometimes stops. Tests then fail with `PoolTimedOut`. Start Docker and run the dev compose again.
- **Disk space on C:.** The Rust `target/` directory grows to tens of GB. When the drive fills up, the linker fails with `LNK1318`. Once only 19 MB was free, and deleting `target/debug/incremental` freed about 27 GB. `cargo clean` is always safe.
- **Parallel linking on Windows.** Running `cargo test --workspace` with full parallelism can exhaust the paging file (os error 1455). Use `cargo test --workspace -j 2`.
- **Two compose projects.** The user's own Adus stack (from the installer) runs under the default compose project name. When starting dev Postgres/Redis, use a separate project name: `docker compose -f docker-compose.dev.yml -p adus-dev up -d`. Never recreate or `down` the user's stack.
- **No browser automation for UI checks.** A REA `capture_browser_scenario` run against the dashboard hung for more than 13 minutes and its Chrome had to be killed. Verify with curl and by reading the code, and say so in reports. The user does click-through testing.

## Code conventions (established in Tasks 1–15)

- **Readable for a Rust learner.** No custom macros and no needless generics. Traits are allowed only at `GitProvider`, `GitProviderFactory`, `LlmClient`, `ChatService`, `JobRunner` (worker seam, `crates/adus/src/worker.rs`) and `RepoCloner` (`crates/review/src/workspace.rs`), each with a real implementation and a test fake. One responsibility per file. Code for the agentic review engine follows the idiomatic standards in Section 5 of its spec (newtypes like `RepoPath`, RAII workspaces, closed enums, typed serde inputs, `thiserror`, no `.unwrap()`/`.expect()` outside tests, `spawn_blocking` for filesystem/regex work); a small generic or trait inside `crates/review` is fine when it removes duplication across the Finder and Verifier loops.
- **SQL.** Use runtime `sqlx::query` / `query_as` with `#[derive(FromRow)]`. Never use the `query!` macros, so builds don't need a database. DB functions return `sqlx::Error`. Tests use `#[sqlx::test(migrations = "../../migrations")]`.
- **citext.** `users.email` and `users.github_login` are `citext`. Bind lookups as `$1::citext`, because a plain `&str` bind compares case-sensitively. Select them with `::text` so they map to `String`.
- **API errors.** Every `/api/v1` error is `{"error":{"code":"<snake_case>","message":"..."}}`. Use `ApiError` from `crates/server/src/error.rs`. Always take JSON bodies with **`ApiJson<T>`**, path params with **`ApiPath<T>`** and query strings with **`ApiQuery<T>`**, never bare `axum::Json`, `Path` or `Query`, so rejections keep the API format. Common codes: `validation_error` (422), `bad_request` (400), `unsupported_media_type` (415), `unauthenticated` (401), `forbidden` (403), `not_found` (404), `last_admin`, `email_taken`, `github_login_taken`, `already_set_up`, `connection_in_use`, `repository_exists` (409), `invalid_current_password` (400), `token_invalid` (422, bad PAT), `provider_error` / `webhook_failed` (502, safe message only), `provider_in_use` (409), `internal` (500, never leaks detail).
- **AppState.** It holds `db`, `cipher`, `config`, `git: Arc<dyn GitProviderFactory>`, `http: reqwest::Client`, `queue` and `chat: Arc<dyn ChatService>` (the same `LlmRouter` the worker's `Engine` uses). `main.rs` builds one `reqwest::Client` with a 30 s timeout and `redirect::Policy::none()`, and uses it for both git and LLM calls. LLM calls set their own 120 s timeout per request. No redirects means a 301 on a POST can't silently turn into a GET.
- **Worker (`crates/adus/src/worker.rs`, exposed as the `adus` lib for tests).** Each delivery runs under lock `pr:{repo_id}:{pr_number}` (TTL 15 min). The job runs in its own `tokio::spawn`, so a panic only fails that job (`Retryable("job panicked")`, bounded by `MAX_ATTEMPTS`) and never kills the worker. `Engine::review_pr` enforces its own `review_timeout` (`REVIEW_TIMEOUT` = 12 min) around the run, so a slow review still finishes its row as `failed` and returns `Fatal("review timed out")` (no retry: it would spend the same LLM tokens again). The worker's 13 min `JOB_TIMEOUT` is only a backstop (it also covers `handle_comment`): on expiry it aborts the task, so the job can't outlive its lock, and returns `Retryable("job timed out")`. Compile-time asserts keep REVIEW_TIMEOUT < JOB_TIMEOUT < LOCK_TTL and JOB_TIMEOUT < `reviews::STALE_AFTER`. Lock busy: `queue.retry` with the attempt unchanged, 30 s. `Retryable`: `queue.retry(attempt + 1, max(retry_after, backoff(attempt)))` while `attempt < MAX_ATTEMPTS`, else `dead_letter`. `Fatal`: `dead_letter`. **Review row lifecycle:** the engine inserts one row per run (`queued` → `running` → `done`/`skipped`/`failed`). On `Fatal` it marks that row `failed`; on `Retryable` it **deletes** its row, because the retry creates a fresh one. When a `Retryable` job runs out of attempts, the worker dead-letters it with `"<error> (gave up after N attempts)"` and calls `JobRunner::gave_up`, which for `Job::ReviewPr` inserts exactly one `failed` row (`reviews::insert_failed`, skipped if the repo is gone). At `adus serve` start, before workers run, `reviews::fail_stale` marks `queued`/`running` rows whose `COALESCE(started_at, created_at)` is older than `adus_db::reviews::STALE_AFTER` (15 min, bound into the SQL) as `failed` ("interrupted (no progress for 15 min)"); `run_scheduler` repeats that sweep every `STALE_SWEEP_EVERY` (5 min, a parameter so tests use a short one) and logs the count when > 0. The engine reviews up to 4 files concurrently (`buffered`, so comment order stays the file order). A per-file `ChatError::Invalid` skips that file (`llm_error`); if every file sent to the LLM is `Invalid`, the review is `Fatal`. GitHub `post_review` errors: rate limit, 5xx and network errors are `Retryable`, other 4xx (`GitError::Status`) are `Fatal`; Azure post errors are always `Fatal`. No model configured: `Opened` and `Command` post the "no model configured" PR comment and end `Fatal`; `Updated` ends `skipped` (`no_model`) with no comment, no failed row and no dead letter. Redis errors in the loop are logged and retried after 1 s. Heartbeat `{prefix}:worker:{name}:hb` EX 30 every 10 s; `Queue::live_workers` lists them. Logs carry job kind, repo id, PR number, attempt and outcome, never job bodies. Shutdown: one `CancellationToken` stops axum, workers and the scheduler; running jobs get 30 s.
- **LLM clients.** All providers share one `send` in `crates/llm/src/types.rs`. It maps 401/403 to `Unauthorized`. 429 maps to `RateLimited`, and `Retry-After` (seconds only) is clamped to at most 3600 s. 5xx, connect, timeout and body-read errors map to `Transient`. 400/404/422 map to `Invalid` with a message cut to 200 chars. Gemini's 400 `API_KEY_INVALID` maps to `Unauthorized`. Key headers are marked sensitive, and error text never contains the key or the URL. The `Openai` kind sends `max_completion_tokens`, because current OpenAI models reject `max_tokens`. `OpenaiCompatible` sends `max_tokens` and requires a `base_url`.
- **Key router (`LlmRouter`, the `ChatService` impl).** It picks candidate keys that are enabled, not `invalid` and not cooling down, in least-recently-used order. It tries each key once. A 429 sets a cooldown (`Retry-After` or 60 s). A 401 marks the key `invalid`. `Transient` moves on to the next key. `Invalid` returns at once without touching the key. Every attempt writes one `llm_usage` row. Status updates are guarded in SQL: nothing changes an `invalid` key, a success never clears a cooldown that is still in the future, and a cooldown never gets shorter. Return values: `NoKeys` means nothing is configured or usable. `AllKeysFailed{retry_after}` covers both "every key failed this time" and "every key is still cooling", and the worker must retry it. `Internal` is a DB error (or no key could be decrypted), displayed as `internal error: …`. A failed decrypt writes a usage row (`error_kind` `decrypt`) and skips the key without changing its status; if no key could be decrypted the router returns `Internal("cannot decrypt API keys — check ADUS_SECRET_KEY")`, so fixing the secret brings every key back.
- **Repository names.** `display_name` is computed on the server only: Azure `project_name/name`, falling back to `owner/name` while `project_name` is NULL; GitHub `owner/name`. `repositories::label` is the single Rust rule and `display_name_sql` its SQL twin; never rebuild it in the web app.
- **Ignore list.** Defaults are Kodus `ignorePaths` verbatim (`adus_core::default_ignore`, AGPL attribution); `settings.ignore_paths = NULL` means "use defaults" (`adus_db::ignore::effective_ignore_paths` / `default_ignore_paths`), reset = set NULL. Matching uses the rule glob semantics (`globset::Glob::new` defaults, `*` crosses `/`) on `normalize_path(path)`, new path only for renames. Saved patterns are trimmed, a leading `/` stripped, empty and duplicates dropped, each ≤ 500 chars, ≤ 5,000 patterns, an invalid one → 422 `invalid_glob` naming it and nothing saved; the whole list is compiled before saving (`IgnoreError::TooComplex` → 422 `invalid_glob`); title keywords ≤ 100 chars. In the engine, deleted files are dropped first (never counted as ignored), then ignored files are skipped before the LLM, not counted toward `max_files_per_review`, and summarised as one `N files ignored` line (never by name). Every file ignored → `skipped` (`nothing_to_review`). An invalid stored list falls back to the defaults. A title keyword match (case-insensitive substring) → `skipped` (`ignored_title`), no comment; `Command` bypasses it.
- **Settings / usage.** `settings` is a single row (`id = 1`). It holds the default provider and model, `max_files_per_review` (50, range 1..=500) and `max_patch_lines_per_file` (2000, range 1..=100000). Usage queries work on UTC calendar days, `to` is inclusive, and missing days are filled with zeros via `generate_series`. In the breakdown, a NULL FK is labelled `(deleted)`. The test-key endpoint writes a usage row and never changes key status. Server tests use `FakeGitFactory` / `FakeGit` from `crates/server/tests/common/mod.rs`. It has a configurable verify result, repo list and errors, plus call counters. Reuse it.
- **Git providers.** Every HTTP call goes through one `send` per provider. That function maps the status and never puts the token, URL or body into an error. GitHub maps 401 to `Unauthorized`, and 429, 403 with `x-ratelimit-remaining: 0`, or 403 with `Retry-After` to `RateLimited`. A plain 403 and any other unmapped failure status is `Status(u16)`; `Http` means no response (network, timeout). GitHub `ensure_webhooks` PATCHes a hook that is inactive or points at another URL (keeping its id), and `webhooks_exist(repo, ids, url)` is true only when every hook is active and points at `url` (both providers), so doctor reports a changed `ADUS_PUBLIC_URL`. Azure maps **203** (sign-in HTML) and 401 to `Unauthorized`. Pagination is capped at 30 pages, with a `tracing::warn!` when truncated. GitHub only follows `Link` next URLs under its own base URL.
- **Azure specifics.** In `RepoRef`, `owner` is the organization, `project` is the **project GUID** and `external_id` is the repo GUID. GUIDs are used in every URL. `connectionData` is called without `api-version`. Diffs are computed locally: file contents come from `items?$format=octetStream` and go through `unified_patch`. A 404 on a side that should exist is an **error**, never an empty patch. `pr_diff` uses the iteration whose `sourceRefCommit` equals `pr.head_sha`. It skips `sourceRename` entries, folders and submodules. Webhooks are one service-hook subscription per event. A subscription that is disabled or has the wrong URL gets recreated. Azure has no HMAC, so the per-repo `webhook_token` goes in the webhook URL as `?token=`.
- **Connections invariant.** Rotating a PAT is: verify the new token, then update **only** the token columns, then re-sync webhooks for each repo. Per-repo webhook errors are returned in `webhook_errors` and don't fail the request. A failed verify changes nothing. Repositories are never touched by rotation. Adding a duplicate active repo is race-safe through `ON CONFLICT` on a partial unique index. Removing a repo soft-deletes it (`deleted_at`).
- **Auth.** The session cookie is `adus_session` (HttpOnly, SameSite=Lax, Path=/, 30 days, `Secure` when `ADUS_PUBLIC_URL` is https). The token is 32 random bytes in base64url. The DB stores only its hex SHA-256. Hash passwords with argon2 inside `spawn_blocking`. Login against an unknown email verifies a dummy hash so timing matches. Extractors are `CurrentUser(User)` and `AdminUser(User)`. Reuse `validate_email`, `validate_password` and the hashing helpers in `auth.rs`.
- **Invariants.** There is always at least one active admin. Demote or disable is guarded in one transaction with `FOR UPDATE`. Disabling a user or resetting their password deletes that user's sessions. A user's own password change keeps the current session.
- **Secrets.** PATs and LLM keys are encrypted with `Cipher` (AES-256-GCM, key `ADUS_SECRET_KEY` = base64 of 32 bytes). API responses only ever include `*_last4` and a status.
- **Paths.** Normalize file paths with no leading `/` (`normalize_path`). Add the `/` back only when calling the Azure API.
- **Comment marker.** Every comment Adus posts contains `<!-- adudu -->`. Comments with that marker are ignored.
- **SPA embedding.** `crates/server/src/spa.rs` uses rust-embed `#[allow_missing = true]` over `web/build`. Unknown `/api/*` and `/webhooks/*` paths return a JSON 404. Other GET/HEAD requests fall back to `index.html` with `no-cache`. `_app/immutable/*` is served with `public, max-age=31536000, immutable`. Release and Docker builds **must build the web app before `cargo build --release`**.
- **Frontend.** Svelte 5 runes and TypeScript, with no state library. The session store is `web/src/lib/session.svelte.ts`, and stale `/auth/me` results are ignored. All calls go through `web/src/lib/api.ts`: prefix `/api/v1`, `credentials: 'include'`, throws `ApiError`, redirects to `/login` on 401 unless `{redirectOn401:false}`. UI copy is in English. SvelteKit 3 replaced `$lib` with `package.json` subpath imports (`#lib` / `#lib/*`). Always import from `#lib/...` (never `$lib/...`) — include the `.js` extension for `.ts` modules (e.g. `#lib/api.js`, `#lib/session.svelte.js`, `#lib/utils.js`) and `.svelte` for components — and do not re-add `alias` to `vite.config.ts` (it triggers `config_option_deprecated_alias` and `tsconfig_invalid` warnings). `web/components.json` configures all `shadcn-svelte` aliases under `#lib`, so newly added UI components use `#lib` natively. Dialogs follow the existing pattern in `web/src/lib/components/*-dialog.svelte`. Destructive actions use an alert-dialog, and notices use sonner toasts. Role `user` sees pages read-only, with controls hidden or disabled via `session.user.role`.
- **Web design & theming.** Any work in `web/` must follow `DESIGN.md` for both light and dark themes. Theme tokens live in `web/src/routes/layout.css` as OKLCH custom properties derived from `DESIGN.md` (warm cream light mode `#fffefb`/`#f8f4f0`/`#201515`, warm espresso dark mode `#201515`/`#2a2320`/`#fffefb`, single `#ff4f00` primary CTA accent — never pure white, pure black, cool greys, or a second chromatic accent). Typography uses self-hosted `@fontsource-variable/mona-sans` (`font-sans` and `font-display`, sentence-case `font-display font-medium` for page `<h1>` headings, uppercase tracked table headers). Shapes use `12px` (`rounded-lg`/`rounded-xl`) for buttons/cards/modals and `6px` (`rounded-sm`) for inputs and sidebar rows. Light/dark/system mode is managed by `mode-watcher` (`ModeWatcher` in `web/src/routes/+layout.svelte`, `ThemeToggle` in `web/src/lib/components/theme-toggle.svelte`, and the pre-paint no-FOUC script in `web/src/app.html`). Reuse semantic tokens (`bg-background`, `bg-card`, `text-foreground`, `text-muted-foreground`, `border-border`, `bg-primary`) and the aligned `shadcn-svelte` primitives instead of hardcoding colors; do not re-enable `richColors` on Sonner toasts, and style empty/admin-only states with the dashed card container.
- **Loading states (skeletons).** Every dashboard page that fetches data shows a skeleton until its first load finishes, and never a blank area or a premature empty state. The pattern:
  - **Flag.** Keep a `loaded` flag (`$state(false)`) that is set to `true` in the load function even when the load fails. Pages with a retry card (Review Rules, Reviews, Settings) also keep a `loadError` flag. System status has no flag; it treats `report === null` as loading.
  - **Branch order.** Branch `{#if loadError}` → `{:else if !loaded}` skeleton → `{:else if items.length === 0}` dashed empty state → real content.
  - **Reloads.** A page whose data depends on a filter or scope resets `loaded = false` when that input changes, as Review Rules and Reviews do. Background polling and post-mutation reloads keep the current rows. Token Usage shows the skeleton only on first load.
  - **Buttons.** Primary header actions ("Add …") are `disabled={!loaded}`. Row actions only appear once the data has loaded.
  - **Tables.** Use `#lib/components/table-skeleton.svelte`:
    - `headers` are the real column labels.
    - `right` lists the right-aligned columns.
    - `card` wraps the table in the same `Card` as the real one.
  - **Other layouts.** Cards, forms and charts use `Skeleton` from `#lib/components/ui/skeleton`, shaped like the real layout (same card, header, badges and button sizes).
  - **Keep skeletons in sync.** When you change a page's layout (add or remove a table column, change a header label, add a card-header action, add a form field, or change card/table nesting), update that page's skeleton in the same change, so the swap from skeleton to data doesn't jump.
  - **New pages.** A new data page must ship with its skeleton.
  - **Testing.** To check skeletons by hand, temporarily make `api()` in `web/src/lib/api.ts` return `new Promise(() => {})` for GETs outside `/auth/`. Never commit that.
- **Commits.** Use Conventional Commits. Each message ends with a `Co-Authored-By:` trailer naming the model that wrote it. Commit on the feature branch and never push without the user asking.

## Product decisions worth knowing (from the spec)

- **Deployment.** One installation is one organization. Only `adus` + Postgres 16 + Redis 7 are needed: no MongoDB, no RabbitMQ.
- **Connections.** A Connection (PAT) is separate from repositories. Rotating a PAT never deletes repos. Each Connection has a "Resync webhooks" action.
- **LLM keys.** One provider can hold any number of keys, including OpenAI-compatible. Keys rotate round-robin. A 429 puts a key in cooldown and a 401 marks it invalid.
- **Rules vs memory.** Rules say what to flag (global or per repo, optional path glob, severity). Memory is always-injected context.
- **Commands.** v1 supports only `@adudu review` and `@adudu remember: …`, matched case-insensitively on the literal text. They are accepted only from registered Adus users: Azure matches by email, GitHub by `users.github_login`. Unregistered authors are ignored silently. Conversational replies are future work.
- **Enterprise code is clean-room.** Never open Kodus `libs/ee/**` or `*.ee.*`. Specify behavior only from public docs and observed behavior (spec 6.13). Non-EE Kodus code is AGPL and can be read as a behavioral reference, but do not port it.

## Clean-room and no-assumption rule (binding for every plan and every agent)

1. **Kodus community code** (https://github.com/kodustech/kodus-ai, anything outside `libs/ee/**` and not named `*.ee.*`) may be read as a behaviour reference and inspiration. Never port or translate it line by line: Adus code, prompts and schemas are written fresh. **Kodus enterprise code** (`libs/ee/**`, `*.ee.*`) is never opened, fetched or searched, not even to look.
2. **Order of authority:** the task brief, then the spec, then this file and the existing Adus code. Kodus community behaviour (or public provider/git docs for wire formats) only fills a gap none of those settles. Never guess from memory of how Kodus or another tool works; if a gap remains, raise it instead of assuming.
3. **Openness:** every implementer report lists its assumptions under an "Assumptions" heading and cites each Kodus community file consulted (path) and the behaviour taken from it.
4. **Reviewers** flag as Important any line-by-line port or translation, anything that looks derived from Kodus enterprise code, uncited Kodus use, and unstated assumptions beyond the brief/spec.
5. **Plans and prompts:** every new plan repeats this rule in its Global Constraints, and every implementer and reviewer subagent prompt carries it verbatim.
