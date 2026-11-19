# AGENTS.md — Adus

Adus is a self-hosted, open-source AI code review platform. Its bot is **`@adudu`**. It reviews pull requests on **Azure DevOps Services** (`dev.azure.com`) and **GitHub**, both connected with a PAT. The backend is Rust and the dashboard is SvelteKit with shadcn-svelte. Adus is inspired by [Kodus](https://github.com/kodustech/kodus-ai), but it is a rewrite, not a port. Licensed AGPL-3.0.

Repo: https://github.com/Return-nol/adus. The GHCR image name is `ghcr.io/return-nol/adus` (lowercase).

## Read these first

- **Design (binding authority):** `docs/superpowers/specs/2026-11-19-adus-design.md`
- **Implementation plan (27 tasks, 6 milestones):** `docs/superpowers/plans/2026-11-19-adus-v1.md`
- **Execution ledger:** `.superpowers/sdd/2026-11-19-adus-v1/progress.md`. This folder is git-ignored and local only. It records which tasks are done, every ruling, deferred minor findings, and the briefs, reports and review diffs for each task.

If the spec and the plan conflict, the spec wins. Record any new decision in the ledger as `Ruling: <what> — <why> — <cost if wrong>`.

## Status (as of 2026-11-19)

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
| 12–27 | LLM clients, key router, AI provider/usage APIs + pages, rules/memory, queue, webhooks, review engine, `@adudu` commands, worker, reviews page, doctor, Docker/installer, CI | not started — **next: Task 12** |

To resume, run superpowers `subagent-driven-development` on the plan. It reads the ledger and continues from the first unfinished task. Implementers have been run on Sonnet at high effort, as the user asked. Deferred minor findings for the final whole-branch review are listed in the ledger (`Task N: minor (deferred)` lines), and so are notes aimed at later tasks (`Note for Task N`).

## Layout

```
crates/core     config, crypto (Cipher, last4), shared types (ConnectionKind, ProviderKind, Severity, normalize_path)
crates/db       sqlx Postgres: connect() + migrations, users.rs, sessions.rs, connections.rs, repositories.rs
crates/git      GitProvider trait + types.rs, diff.rs, github.rs, azure.rs, HttpGitProviderFactory (lib.rs)
crates/llm      (Task 12+) LlmClient per provider, multi-key router
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
cargo run -p adus                                  # http://localhost:8080; export the variables from .env first
```

On Windows, if `cargo` is not on PATH, it is in `%USERPROFILE%\.cargo\bin`. The rustup MSVC toolchain is used.

## Environment gotchas

- **Dev ports.** The host already runs native Postgres on 5432 and Redis on 6379, so the dev compose maps to **55432** and **55379**. Port 56379 cannot be used because it is inside a Windows reserved TCP range. Do not stop the user's native services.
- **Docker Desktop.** It sometimes stops. Tests then fail with `PoolTimedOut`. Start Docker and run the dev compose again.
- **No browser automation for UI checks.** A REA `capture_browser_scenario` run against the dashboard hung for more than 13 minutes and its Chrome had to be killed. Verify with curl and by reading the code, and say so in reports. The user does click-through testing.

## Code conventions (established in Tasks 1–11)

- **Readable for a Rust learner.** No custom macros and no needless generics. Traits are allowed only at `GitProvider`, `GitProviderFactory`, `LlmClient` and `ChatService`, each with a real implementation and a test fake. One responsibility per file.
- **SQL.** Use runtime `sqlx::query` / `query_as` with `#[derive(FromRow)]`. Never use the `query!` macros, so builds don't need a database. DB functions return `sqlx::Error`. Tests use `#[sqlx::test(migrations = "../../migrations")]`.
- **citext.** `users.email` and `users.github_login` are `citext`. Bind lookups as `$1::citext`, because a plain `&str` bind compares case-sensitively. Select them with `::text` so they map to `String`.
- **API errors.** Every `/api/v1` error is `{"error":{"code":"<snake_case>","message":"..."}}`. Use `ApiError` from `crates/server/src/error.rs`. Always take JSON bodies with **`ApiJson<T>`**, path params with **`ApiPath<T>`** and query strings with **`ApiQuery<T>`**, never bare `axum::Json`, `Path` or `Query`, so rejections keep the API format. Common codes: `validation_error` (422), `bad_request` (400), `unsupported_media_type` (415), `unauthenticated` (401), `forbidden` (403), `not_found` (404), `last_admin`, `email_taken`, `github_login_taken`, `already_set_up`, `connection_in_use`, `repository_exists` (409), `invalid_current_password` (400), `token_invalid` (422, bad PAT), `provider_error` / `webhook_failed` (502, safe message only), `internal` (500, never leaks detail).
- **AppState.** It holds `db`, `cipher`, `config` and `git: Arc<dyn GitProviderFactory>`. Later tasks add `queue` (Task 19, first consumer) and `chat` (Task 23). `main.rs` builds the git factory on a `reqwest::Client` with a 30 s timeout and `redirect::Policy::none()`, so a 301 on a POST can't silently turn into a GET. Server tests use `FakeGitFactory` / `FakeGit` from `crates/server/tests/common/mod.rs`. It has a configurable verify result, repo list and errors, plus call counters. Reuse it.
- **Git providers.** Every HTTP call goes through one `send` per provider. That function maps the status and never puts the token, URL or body into an error. GitHub maps 401 to `Unauthorized`, and 429, 403 with `x-ratelimit-remaining: 0`, or 403 with `Retry-After` to `RateLimited`. A plain 403 is `Http`. Azure maps **203** (sign-in HTML) and 401 to `Unauthorized`. Pagination is capped at 30 pages, with a `tracing::warn!` when truncated. GitHub only follows `Link` next URLs under its own base URL.
- **Azure specifics.** In `RepoRef`, `owner` is the organization, `project` is the **project GUID** and `external_id` is the repo GUID. GUIDs are used in every URL. `connectionData` is called without `api-version`. Diffs are computed locally: file contents come from `items?$format=octetStream` and go through `unified_patch`. A 404 on a side that should exist is an **error**, never an empty patch. `pr_diff` uses the iteration whose `sourceRefCommit` equals `pr.head_sha`. It skips `sourceRename` entries, folders and submodules. Webhooks are one service-hook subscription per event. A subscription that is disabled or has the wrong URL gets recreated. Azure has no HMAC, so the per-repo `webhook_token` goes in the webhook URL as `?token=`.
- **Connections invariant.** Rotating a PAT is: verify the new token, then update **only** the token columns, then re-sync webhooks for each repo. Per-repo webhook errors are returned in `webhook_errors` and don't fail the request. A failed verify changes nothing. Repositories are never touched by rotation. Adding a duplicate active repo is race-safe through `ON CONFLICT` on a partial unique index. Removing a repo soft-deletes it (`deleted_at`).
- **Auth.** The session cookie is `adus_session` (HttpOnly, SameSite=Lax, Path=/, 30 days, `Secure` when `ADUS_PUBLIC_URL` is https). The token is 32 random bytes in base64url. The DB stores only its hex SHA-256. Hash passwords with argon2 inside `spawn_blocking`. Login against an unknown email verifies a dummy hash so timing matches. Extractors are `CurrentUser(User)` and `AdminUser(User)`. Reuse `validate_email`, `validate_password` and the hashing helpers in `auth.rs`.
- **Invariants.** There is always at least one active admin. Demote or disable is guarded in one transaction with `FOR UPDATE`. Disabling a user or resetting their password deletes that user's sessions. A user's own password change keeps the current session.
- **Secrets.** PATs and LLM keys are encrypted with `Cipher` (AES-256-GCM, key `ADUS_SECRET_KEY` = base64 of 32 bytes). API responses only ever include `*_last4` and a status.
- **Paths.** Normalize file paths with no leading `/` (`normalize_path`). Add the `/` back only when calling the Azure API.
- **Comment marker.** Every comment Adus posts contains `<!-- adudu -->`. Comments with that marker are ignored.
- **SPA embedding.** `crates/server/src/spa.rs` uses rust-embed `#[allow_missing = true]` over `web/build`. Unknown `/api/*` and `/webhooks/*` paths return a JSON 404. Other GET/HEAD requests fall back to `index.html` with `no-cache`. `_app/immutable/*` is served with `public, max-age=31536000, immutable`. Release and Docker builds **must build the web app before `cargo build --release`**.
- **Frontend.** Svelte 5 runes and TypeScript, with no state library. The session store is `web/src/lib/session.svelte.ts`, and stale `/auth/me` results are ignored. All calls go through `web/src/lib/api.ts`: prefix `/api/v1`, `credentials: 'include'`, throws `ApiError`, redirects to `/login` on 401 unless `{redirectOn401:false}`. UI copy is in English. SvelteKit 3 replaced `$lib` with `#lib`, but shadcn-svelte generates `$lib` imports, so `vite.config.ts` keeps an `alias: { $lib: 'src/lib' }` workaround. The deprecation notice it prints is expected. Dialogs follow the existing pattern in `web/src/lib/components/*-dialog.svelte`. Destructive actions use an alert-dialog, and notices use sonner toasts. Role `user` sees pages read-only, with controls hidden or disabled via `session.user.role`.
- **Commits.** Use Conventional Commits. Each message ends with a `Co-Authored-By:` trailer naming the model that wrote it. Commit on the feature branch and never push without the user asking.

## Product decisions worth knowing (from the spec)

- **Deployment.** One installation is one organization. Only `adus` + Postgres 16 + Redis 7 are needed: no MongoDB, no RabbitMQ.
- **Connections.** A Connection (PAT) is separate from repositories. Rotating a PAT never deletes repos. Each Connection has a "Resync webhooks" action.
- **LLM keys.** One provider can hold any number of keys, including OpenAI-compatible. Keys rotate round-robin. A 429 puts a key in cooldown and a 401 marks it invalid.
- **Rules vs memory.** Rules say what to flag (global or per repo, optional path glob, severity). Memory is always-injected context.
- **Commands.** v1 supports only `@adudu review` and `@adudu remember: …`, matched case-insensitively on the literal text. They are accepted only from registered Adus users: Azure matches by email, GitHub by `users.github_login`. Unregistered authors are ignored silently. Conversational replies are future work.
- **Enterprise code is clean-room.** Never open Kodus `libs/ee/**` or `*.ee.*`. Specify behavior only from public docs and observed behavior (spec 6.13). Non-EE Kodus code is AGPL and can be read as a behavioral reference, but do not port it.
