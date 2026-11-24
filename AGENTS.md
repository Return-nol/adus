# AGENTS.md — Adus

Adus is a self-hosted, open-source (AGPL-3.0) AI code review platform (`@adudu`) for **Azure DevOps Services** (`dev.azure.com`) and **GitHub**, connected with a PAT. Rust backend + SvelteKit/shadcn-svelte SPA. Inspired by [Kodus](https://github.com/kodustech/kodus-ai) — a fresh rewrite, never a port.

Repo: https://github.com/Return-nol/adus · GHCR image: `ghcr.io/return-nol/adus` (lowercase)

## Read these first & Authority Order

1. **Task brief / active change spec** (any unmerged feature spec in `docs/superpowers/specs/` currently being implemented)
2. **Living technical spec (binding):** [SPEC.md](docs/SPEC.md)
3. **Web visual design (binding for `web/`):** [DESIGN.md](docs/DESIGN.md) · **Web conventions:** [web/AGENTS.md](web/AGENTS.md)
4. **Existing Adus code** and this file
5. **Backlog of deferred minor findings:** [docs/superpowers/backlog.md](docs/superpowers/backlog.md)

Historical dated specs and plans live under `docs/superpowers/specs/` and `docs/superpowers/plans/`. If a spec and a plan conflict, the spec wins.

## Workflow & SDD Completion Rule (Mandatory)

For new work, follow `brainstorming` → `writing-plans` → `subagent-driven-development` (SDD). Every plan and subagent prompt must carry the **Clean-room and no-assumption rule** below.

**Whenever SDD (or any feature/spec-driven change) completes, before committing the final state or merging:**

1. **Update [docs/SPEC.md](docs/SPEC.md)** so it reflects all implemented behavior, schema migrations, API endpoints, settings, and invariants introduced by the branch (and update [DESIGN.md](docs/DESIGN.md) if visual tokens or UI patterns changed).
2. **Mark the dated change spec** (`docs/superpowers/specs/YYYY-MM-DD-*.md`) as `Status: implemented` (keep the file as the historical ADR/delta) and add/update its entry in Section 10 of [docs/SPEC.md](docs/SPEC.md).
3. **Update `AGENTS.md` / [web/AGENTS.md](web/AGENTS.md) selectively** — only add or adjust cross-cutting guardrails, commands, or environment gotchas. Keep subsystem details in [docs/SPEC.md](docs/SPEC.md) so `AGENTS.md` stays concise and never truncates.
4. **Plan & reviewer enforcement:** Every new implementation plan must include **"Sync `docs/SPEC.md` and mark spec implemented"** as its final task, and SDD/whole-branch reviewers must flag missing `docs/SPEC.md` updates on behavior-, schema-, or API-changing branches as **Important**.

## Layout

```text
crates/core     config, AES-256-GCM cipher, default ignore paths, shared types (ReviewMode, Job, normalize_path)
crates/db       sqlx Postgres: connect(), embedded migrations/, queries (users, sessions, connections, repositories, ai, settings, usage, reviews)
crates/git      GitProvider, GitProviderFactory, CloneSpec, diff utilities, GitHub & Azure clients
crates/llm      LlmClient, OpenAI / Anthropic / Gemini / OpenAI-compatible clients, ReasoningEffort, LlmRouter (ChatService)
crates/queue    Redis Streams job queue, delivery dedup, per-PR lock, worker heartbeats
crates/review   workspace (GitCliCloner, RepoWorkspace, RepoPath), tools, redact, repo_instructions, agent, specialists, finder, verifier, dedup, budget, small_pr, prompt, parse, command, engine
crates/server   axum: router(), state.rs, error.rs, auth.rs, spa.rs, webhooks.rs, doctor.rs, api/*
crates/adus     binary (serve, gen-secret, check-config, doctor, healthcheck), worker loop & scheduler
migrations/     sqlx migrations (0001..0008), embedded by crates/db
web/            SvelteKit SPA (adapter-static) + shadcn-svelte; web/build is embedded into the binary
```

Dependencies must stay acyclic: `core` < `db`/`git`/`queue`; `llm` uses `core` + `db`; `review` uses `core` + `db` + `git` + `llm` + `queue`; `server` and `adus` use everything.

## Commands

```sh
docker compose -f docker-compose.dev.yml -p adus-dev up -d   # Postgres → localhost:55432, Redis → localhost:55379
cargo test --workspace -j 2                                 # env from .cargo/config.toml; -j 2 avoids Windows paging exhaustion
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
pnpm -C web install
pnpm -C web check                                           # must report 0 errors
pnpm -C web build                                           # required before spa tests and release builds
pnpm -C web dev                                             # http://localhost:5173, proxies /api and /webhooks to :8000
ADUS_PUBLIC_URL=http://localhost:8000 cargo run -p adus     # http://localhost:8000
```

CI (`.github/workflows/ci.yml`): `rust` (`fmt`, `clippy`, `cargo test` with Postgres/Redis) and `web` (`check`, `fmt:check`). Release (`.github/workflows/release.yml`): pushes multi-arch image on `v*` tags. On Windows, if `cargo` is missing from `PATH`, check `%USERPROFILE%\.cargo\bin` (MSVC toolchain).

## Environment Gotchas

- **Dev ports:** Native Postgres (5432) and Redis (6379) run on this host; dev compose maps to **55432** and **55379** (never 56379, reserved on Windows). Do not stop native services.
- **Two compose projects:** Always pass `-p adus-dev` with `docker-compose.dev.yml`. Never recreate or `down` the user's own default-project Adus stack.
- **Docker Desktop:** If tests fail with `PoolTimedOut`, start Docker Desktop and re-run dev compose.
- **Disk space & linking on Windows:** `target/` grows rapidly (`LNK1318` if C: fills; deleting `target/debug/incremental` or running `cargo clean` recovers space). Always pass `-j 2` to `cargo test` to avoid `os error 1455`.
- **No browser automation for UI checks:** Headless Chrome automation previously hung >13 min against this dashboard. Verify UI changes via `pnpm -C web check`, code inspection, and `curl` against `/api/v1/*`, and state this explicitly in reports.

## Cross-Cutting Code Conventions

Full subsystem specs live in [docs/SPEC.md](docs/SPEC.md). Follow these standing rules everywhere:

- **Rust style & trait seams:** Write clear, idiomatic Rust (Section 5 of the agentic spec / [docs/SPEC.md §2.3](docs/SPEC.md#23-rust-standards)): newtypes (`RepoPath`), RAII (`RepoWorkspace`), closed enums, `thiserror`, typed serde structs, **zero `.unwrap()`/`.expect()` outside tests**, `spawn_blocking` for filesystem/regex/argon2, and never hold `std::sync::RwLock` across `.await`. Only use `dyn Trait` at the 6 seams (`GitProvider`, `GitProviderFactory`, `LlmClient`, `ChatService`, `JobRunner`, `RepoCloner`), each with a real impl and test fake.
- **SQL & `citext`:** Use runtime `sqlx::query` / `query_as` + `#[derive(FromRow)]` — never `query!` macros. Bind `users.email` / `users.github_login` (`citext`) lookups as `$1::citext` and select with `::text`. DB functions return `sqlx::Error`. DB tests use `#[sqlx::test(migrations = "../../migrations")]`.
- **API contract:** Every `/api/v1` error uses `ApiError` (`{"error":{"code":"<snake_case>","message":"..."}}`). Always extract with **`ApiJson<T>`**, **`ApiPath<T>`**, and **`ApiQuery<T>`** — never bare `axum::Json`, `Path`, or `Query`. Server tests reuse `FakeGitFactory` / `FakeGit` in `crates/server/tests/common/mod.rs`.
- **Security & secrets:** Never expose decrypted PATs or LLM keys in API responses, logs, error strings, git argv, or `.git/config` (only `*_last4` and status). Mark key headers sensitive. Read-only tools must go through `RepoWorkspace::resolve` (`RepoPath`) and `redact_secrets` before caching/posting.
- **Shared invariants:**
  - `display_name` is server-only (`repositories::label` / `display_name_sql`).
  - Normalize repo paths with `normalize_path` (no leading `/`; add `/` only for Azure API calls).
  - Every comment Adus posts carries `<!-- adudu -->`, and comments containing it are ignored.
  - Keep worker/engine timeout hierarchy intact (`REVIEW_TIMEOUT < SPECIALISTS_REVIEW_TIMEOUT < JOB_TIMEOUT < LOCK_TTL / RECLAIM_IDLE / STALE_AFTER`, enforced by compile-time asserts).
  - Never send a saved LLM key to a modified `base_url`; model-list cache uses HMAC under `ADUS_SECRET_KEY`.
- **Frontend (`web/`):** Read [web/AGENTS.md](web/AGENTS.md) and [DESIGN.md](docs/DESIGN.md) before touching `web/`. Always use `#lib/...` (never `$lib`), follow [DESIGN.md](docs/DESIGN.md) OKLCH theming, and ship matching skeletons on every data page. Release/Docker builds must run `pnpm -C web build` before `cargo build --release`.
- **Testing (no redundant tests):** Check existing tests before writing new ones; extend or parameterize existing cases instead of duplicating them. Do not write unit tests that mirror coverage already verified by integration tests or that only test trivial wrappers, getters, or `serde` derives — every test must cover a distinct behavior, edge case, invariant, or regression. Reuse existing fakes/fixtures (`FakeGitFactory`/`FakeGit`, `crates/review/tests/common/mod.rs`, `#[sqlx::test(migrations = "../../migrations")]`).
- **Commits:** Commit on the feature branch; never push without the user asking.

## Clean-Room and No-Assumption Rule (Binding for Every Plan and Every Agent)

1. **Kodus community code** (https://github.com/kodustech/kodus-ai, anything outside `libs/ee/**` and not named `*.ee.*`) may be read as a behaviour reference and inspiration. Never port or translate it line by line: Adus code, prompts, and schemas are written fresh. **Kodus enterprise code** (`libs/ee/**`, `*.ee.*`) is never opened, fetched, or searched, not even to look.
2. **Order of authority:** the task brief (or active unmerged change spec), then [docs/SPEC.md](docs/SPEC.md) (and [DESIGN.md](docs/DESIGN.md) for `web/`), then this file and the existing Adus code. Kodus community behaviour (or public provider/git docs for wire formats) only fills a gap none of those settles. Never guess from memory of how Kodus or another tool works; if a gap remains, raise it instead of assuming.
3. **Openness:** every implementer report lists its assumptions under an `"Assumptions"` heading and cites each Kodus community file consulted (path) and the behaviour taken from it.
4. **Reviewers** flag as **Important** any line-by-line port or translation, anything that looks derived from Kodus enterprise code, uncited Kodus use, unstated assumptions beyond the brief/spec, redundant unit tests, and missing [docs/SPEC.md](docs/SPEC.md) updates when SDD completes.
5. **Plans and prompts:** every new plan repeats this rule in its Global Constraints, and every implementer and reviewer subagent prompt carries it verbatim.
