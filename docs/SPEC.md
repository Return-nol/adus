# Adus — Living System Specification

> **Canonical authority:** This document is the single living technical specification for **Adus**, consolidating and superseding the dated design specs under `docs/superpowers/specs/`.
> - **Web visual system (binding for `web/`):** [DESIGN.md](./DESIGN.md)
> - **Agent operating rules:** [AGENTS.md](../AGENTS.md) and [web/AGENTS.md](../web/AGENTS.md)
> - **Active change specs:** Any unmerged feature spec in `docs/superpowers/specs/` governs work on that feature until merged, at which point its delta is folded here and the dated spec is marked `Status: implemented`.

---

## 1. Overview & Product Decisions

Adus is a self-hosted, open-source (AGPL-3.0) AI code review platform inspired by [Kodus](https://github.com/kodustech/kodus-ai) (written fresh in Rust + SvelteKit, not ported). Its review bot is **`@adudu`**.

- **Deployment footprint:** Single Rust binary (`adus`, serving HTTP + background worker) + **Postgres 18** + **Redis 8** (`appendonly yes`). No MongoDB, RabbitMQ, or separate sandbox containers.
- **Single-tenant installation:** One installation equals one organization. All users, connections, repositories, AI providers, rules, and memories belong to that installation.
- **Supported Git hosts (via PAT):** **GitHub** (and GitHub Enterprise Server) and **Azure DevOps Services** (`dev.azure.com`). GitLab, Bitbucket, Forgejo, Azure DevOps Server (on-prem), SSO/SAML, OAuth login, conversational `@adudu` replies, and USD cost calculation are out of scope.
- **Connections vs. Repositories:** A `Connection` (PAT) is decoupled from `Repository` rows. Rotating a PAT verifies the new token, updates only token columns, and resyncs repo webhooks without deleting repositories, reviews, rules, or memories.
- **Multi-key AI Providers:** Each `AiProvider` (`openai`, `anthropic`, `gemini`, `openai_compatible`) holds a required `primary_model`, an optional `fast_model`, and any number of API keys rotated round-robin/LRU with automatic cooldowns and invalidation.
- **Rules vs. Memory:**
  - **Rules** define what to flag (`global` or `repo` scope, optional `path_glob`, severity `info | warning | critical`).
  - **Memory** is always-injected team context (`global` or `repo` scope), created in the dashboard or via `@adudu remember: ...` in a PR comment. No automatic conflict/dedup resolution.
- **Clean-room policy:** Kodus non-EE code (outside `libs/ee/**` and `*.ee.*`) may be consulted as behavioral reference with citation, never ported line-by-line. Kodus Enterprise code (`libs/ee/**`, `*.ee.*`) must never be opened, fetched, or searched; EE-equivalent features are specified solely from public docs and observed behavior.

---

## 2. Architecture & Engineering Standards

### 2.1 Crate Workspace Layout

Dependencies remain strictly acyclic:

```text
crates/core     config, AES-256-GCM cipher (Cipher, last4), default ignore list, shared domain types & ReviewMode, Job payloads, normalize_path
crates/db       sqlx Postgres pool + embedded migrations (0001..0008), users, sessions, connections, repositories, ai, settings, usage, reviews
crates/git      GitProvider trait, GitProviderFactory, CloneSpec, diff utilities (commentable_lines, commentable_hunks, unified_patch), GitHub & Azure clients
crates/llm      LlmClient trait, OpenAI / Anthropic / Gemini / OpenAI-compatible clients, ReasoningEffort, LlmRouter (ChatService)
crates/queue    Redis Streams job queue (XADD / XREADGROUP / XAUTOCLAIM), delivery dedup, per-PR lock, worker heartbeats
crates/review   workspace (GitCliCloner, RepoWorkspace, RepoPath), tools, redact, repo_instructions, agent, specialists, finder, verifier, dedup, budget, small_pr, prompt, parse, command, engine
crates/server   axum REST API (/api/v1/*), webhook handlers (/webhooks/*), auth extractors, doctor checks, embedded SPA (rust-embed)
crates/adus     binary entrypoint (serve, gen-secret, check-config, doctor, healthcheck), worker loop & scheduler
migrations/     embedded SQL migrations
web/            SvelteKit static SPA + shadcn-svelte (built to web/build and embedded)
```

Dependency order:
- `core` sits under `db`, `git`, and `queue`.
- `llm` depends on `core` and `db`.
- `review` depends on `core`, `db`, `git`, `llm`, and `queue`.
- `server` and `adus` depend on the workspace crates.

### 2.2 Runtime & HTTP Client

- `crates/adus/src/main.rs` constructs one `reqwest::Client` (30 s default timeout, `redirect::Policy::none()` so a 301 on a POST cannot silently become a GET) shared across git and LLM clients.
- Individual LLM requests override timeout per call: **120 s**, or **300 s** when `ReasoningEffort` is set.
- `AppState` holds `db`, `cipher`, `config`, `git: Arc<dyn GitProviderFactory>`, `http: reqwest::Client`, `queue`, and `chat: Arc<dyn ChatService>` (the same `LlmRouter` used by the review `Engine`).

### 2.3 Rust Standards

- **Trait seams only where needed:** `GitProvider`, `GitProviderFactory`, `LlmClient`, `ChatService`, `JobRunner` (`crates/adus/src/worker.rs`), and `RepoCloner` (`crates/review/src/workspace.rs`) — each with a real implementation and a test fake. Internal dispatch uses closed enums (`SpecialistKind`, `ToolKind`) and static dispatch.
- **Type-driven invariants:** `RepoPath` (constructible only via `RepoWorkspace::resolve`), RAII `RepoWorkspace`, typed serde tool/output structs, `thiserror` error enums, and **zero `.unwrap()` or `.expect()`** outside tests.
- **Async discipline:** Synchronous filesystem walks, regex searches, diffing, and argon2 hashing run in `tokio::task::spawn_blocking`. `std::sync::RwLock` guards (e.g., `ToolCache`) are never held across `.await`.
- **SQL discipline:** Runtime `sqlx::query` / `sqlx::query_as` with `#[derive(FromRow)]` (never compile-time `query!` macros). Tests use `#[sqlx::test(migrations = "../../migrations")]`.

---

## 3. Data Model & Security Invariants

### 3.1 Database Schema (`migrations/0001` – `0008`)

- `users`: `id`, `email` (`citext`), `password_hash`, `role` (`admin | user`), `disabled`, `github_login` (`citext` nullable), `created_at`.
  - Always bind `citext` lookups as `$1::citext` and select as `::text`.
  - **Invariant:** At least one active `admin` must always exist (demote/disable guarded in a transaction with `FOR UPDATE`).
- `sessions`: `token_hash` (hex SHA-256 of 32-byte base64url cookie token), `user_id`, expiry (30 days).
  - Disabling a user or resetting their password deletes all their sessions; changing one's own password preserves the current session.
- `connections`: `id`, `kind` (`azure_devops | github`), `name`, `org_or_owner`, `base_url`, `encrypted_token`, `token_last4`, `status` (`ok | invalid | unverified`), `last_verified_at`, `pat_owner`.
- `repositories`: `id`, `connection_id`, `external_id`, `project` (Azure project GUID), `project_name` (Azure project display name, nullable, migration 0006), `name`, `default_branch`, `enabled`, `auto_review`, `webhook_external_id`, `webhook_secret`, `review_provider_id` (nullable), `review_mode` (nullable `review_mode`, migration 0008), `deleted_at` (soft-delete).
  - Duplicate active repos prevented via `ON CONFLICT` on a partial unique index.
  - **Display name invariant:** Computed server-side only via `repositories::label` (Rust) and `display_name_sql` (SQL): Azure `project_name/name` (fallback `owner/name` if `project_name` is `NULL`), GitHub `owner/name`. Never reconstructed in `web/`.
- `ai_providers`: `id`, `kind` (`openai | anthropic | gemini | openai_compatible`), `name`, `base_url`, `primary_model` (nullable in DB after migration 0008 = "needs setup", required on create), `fast_model` (nullable; `effective_fast_model()` falls back to `primary_model`), `enabled`.
- `ai_keys`: `id`, `provider_id`, `label`, `encrypted_key`, `key_last4`, `enabled`, `status` (`ok | invalid | rate_limited`), `cooldown_until`, `last_used_at`.
- `settings` (single row `id = 1`): `default_provider_id`, `ignore_paths` (`text[] NULL`, `NULL` = Kodus defaults, migration 0007), `ignored_title_keywords` (`text[] NOT NULL DEFAULT '{}'`), `review_mode` (`generalist | specialists`, default `generalist`), `max_files_per_review` (default `30`, `1..=500`), `max_patch_lines_per_file` (default `1500`, `1..=100000`), `small_pr_max_lines` (default `100`, `0..=10000`, `0` = off), `max_review_input_tokens` (default `2_000_000`, `0..=100_000_000`, `0` = no limit).
- `rules`: `id`, `scope` (`global | repo`), `repository_id`, `title`, `body`, `path_glob`, `severity` (`info | warning | critical`), `enabled`.
- `memories`: `id`, `scope` (`global | repo`), `repository_id`, `content`, `source` (`manual | pr_comment`), `source_url`, `created_by`, `enabled`.
- `reviews`: `id`, `repository_id`, `pr_number`, `base_sha`, `head_sha`, `trigger` (`opened | updated | command`), `status` (`queued | running | done | failed | skipped`), `error`, `started_at`, `finished_at`, `comments_posted`.
- `llm_usage`: `id`, `review_id`, `repository_id`, `provider_id`, `key_id`, `model`, `input_tokens` (total input, cached included), `output_tokens`, `cache_read_tokens`, `cache_write_tokens`, `latency_ms`, `success`, `error_kind`, `created_at`.

### 3.2 Encryption & Secret Handling

- PATs and LLM keys are encrypted at rest using `adus_core::crypto::Cipher` (AES-256-GCM, 32-byte base64 key from `ADUS_SECRET_KEY`).
- API responses only ever expose `*_last4` and status.
- Key headers are marked sensitive (`reqwest::header::HeaderValue::set_sensitive`); HTTP/LLM error messages never include tokens, keys, URLs with secrets, or raw response bodies.
- Model-discovery Redis cache keys (`{prefix}:cache:models:v2:<hex>`, 1 h TTL) hash `(kind, normalized_base_url, api_key)` via HMAC-SHA256 keyed by `ADUS_SECRET_KEY` — never a bare hash of an API key. Stored keys are only ever sent to their provider's saved `base_url`.

---

## 4. Auth, REST API & SPA Serving (`crates/server`)

### 4.1 Authentication & RBAC

- First user to register via `/setup` becomes `admin`; subsequent public registration is closed and admins create users.
- Cookie: `adus_session` (`HttpOnly`, `SameSite=Lax`, `Path=/`, 30-day max-age, `Secure` when `ADUS_PUBLIC_URL` is `https://`).
- Passwords hashed/verified with `argon2` in `spawn_blocking`. Login against an unknown email verifies a dummy hash to prevent timing side-channels.
- Route extractors: `CurrentUser(User)` and `AdminUser(User)`.
  - `user` role: read-only access across pages + full management of `rules` and `memories` + own password/profile + viewing token usage.
  - `admin` role: everything plus `connections`, `repositories`, `ai_providers`/`ai_keys`, `settings`, `users`, and `admin/doctor`.

### 4.2 API Contract & SPA

- Every `/api/v1/*` error returns `{"error":{"code":"<snake_case>","message":"..."}}` via `ApiError`.
- Handlers always use `ApiJson<T>`, `ApiPath<T>`, and `ApiQuery<T>` (never bare `axum::Json`, `Path`, or `Query`).
- `crates/server/src/spa.rs` embeds `web/build` via `rust-embed` (`#[allow_missing = true]`). Unknown `/api/*` and `/webhooks/*` routes return JSON 404; other `GET`/`HEAD` requests serve `index.html` with `Cache-Control: no-cache`; `_app/immutable/*` is served with `public, max-age=31536000, immutable`.
- Release/Docker builds must run `pnpm -C web build` before `cargo build --release`.

---

## 5. Git Providers & Webhooks (`crates/git`)

- Every HTTP request passes through a single `send` helper per provider (`github.rs`, `azure.rs`) that strips tokens/URLs from errors and enforces pagination capped at 30 pages (`tracing::warn!` on truncation).
- **Path normalization:** File paths are normalized without a leading `/` (`normalize_path`); a leading `/` is added only when calling Azure APIs.
- **GitHub:**
  - Maps `401` → `Unauthorized`; `429`, `403` with `x-ratelimit-remaining: 0`, or `403` with `Retry-After` → `RateLimited`; other unmapped errors → `Status(u16)`; network/timeout → `Http`.
  - Follows `Link` pagination only under its own base URL.
  - Webhooks: `pull_request`, `issue_comment`, `pull_request_review_comment`, verified via `X-Hub-Signature-256` HMAC. `ensure_webhooks` PATCHes an inactive or mismatched-URL hook (preserving its ID). `edited` webhook with a title change re-triggers a review.
  - `CloneSpec`: `refs/pull/{pr.number}/head`, `Basic x-access-token:{pat}`.
- **Azure DevOps Services:**
  - `RepoRef.owner` = organization, `RepoRef.project` = project **GUID**, `RepoRef.external_id` = repo GUID. Every URL uses GUIDs.
  - `connectionData` is called **without** `api-version`. Maps both **203** (sign-in HTML) and **401** to `Unauthorized`.
  - Diffs are computed locally: file contents fetched via `items?$format=octetStream` and diffed via `similar` (`unified_patch`). A `404` on an expected side is an error (never an empty patch). `pr_diff` selects the iteration where `sourceRefCommit == pr.head_sha`, skipping `sourceRename`, folders, and submodules.
  - Webhooks: one service hook subscription per event (`git.pullrequest.created`, `git.pullrequest.updated`, `ms.vss-code.git-pullrequest-comment-event`), authenticated via `?token={webhook_secret}` in the URL. Inactive or wrong-URL subscriptions are recreated.
  - `CloneSpec`: `pr.head_sha` with `fallback_refspec = pr.source_ref`, `Basic :{pat}`.
- **`webhooks_exist(repo, ids, url)`:** True on both providers only when every hook/subscription is active and targets `url`.
- **Multi-line inline comments:**
  - `InlineComment` carries optional `start_line` and `end_offset` (`start_line..=line` is the span of existing RIGHT-side lines replaced by a suggestion).
  - GitHub sends `start_line` + `start_side: "RIGHT"` when `start_line < line`.
  - Azure sets `rightFileStart { line: start_line, offset: 1 }` and `rightFileEnd { line, offset: end_offset }` (UTF-16 code-unit length + 1; single-line uses `offset: 1`).

---

## 6. AI Providers, Reasoning Effort & Key Router (`crates/llm`)

### 6.1 `LlmClient` & Wire Mapping

- All providers share one `send` helper in `crates/llm/src/types.rs`:
  - `401`/`403` (and Gemini `400 API_KEY_INVALID`) → `Unauthorized`.
  - `429` → `RateLimited` (`Retry-After` seconds clamped to `3600` s).
  - `5xx`, connect, timeout, body-read → `Transient`.
  - `400`/`404`/`413`/`422` → `Invalid` (message truncated to 200 chars), **except**:
    - Tools markers (read only from `error.message`/`code`/`type` or string `error`, never echoed input) when `req.tools` is non-empty → `ToolsUnsupported`.
    - `413` or overflow markers (never on `404`) → `ContextOverflow`.
- **Provider quirks:**
  - `Openai` sends `max_completion_tokens`; `OpenaiCompatible` sends `max_tokens` and requires `base_url`. OpenAI-compatible tool calls lacking an `id` receive `call_{prior}_{index}`.
  - `Anthropic` places `cache_control: {"type": "ephemeral"}` on system blocks, the last tool definition, and the last block of the latest user/tool message (≤ 4 breakpoints), maps `Message::Tool` to `"role": "user"`, preserves `Thinking`/`RedactedThinking` blocks in `provider_meta`, and computes `input_tokens = input + cache_read + cache_write` so usage matches OpenAI/Gemini.
  - `Gemini` maps `Message::Assistant` to `"role": "model"`, preserves `thoughtSignature` in `provider_meta`, ignores `part.thought == true` when building response text, and assigns deterministic IDs `gc_{prior}_{idx}`.
- **Reasoning effort (`ReasoningEffort::{Low, Medium, High}`):**
  - Adds an output token reserve (`Low: 2048`, `Medium: 4096`, `High: 8192`; Gemini thinking budgets `1024 / 4096 / 8192`) and raises request timeout to 300 s.
  - If `Invalid` matches `REASONING_UNSUPPORTED_MARKERS`, `chat` transparently retries once with `effort: None` (120 s timeout).
- **Model discovery (`list_models`):** OpenAI/compatible `/models`, Anthropic `/v1/models`, Gemini `/v1beta/models` (filtered to `generateContent`, `models/` prefix stripped), trimmed, sorted, deduplicated.

### 6.2 Multi-Key Router (`LlmRouter`)

- Selects candidate keys (`enabled`, not `invalid`, not cooling down) in **LRU** order; tries each key once per call.
- `RateLimited` → cooldown (`Retry-After` or 60 s); `Unauthorized` → `invalid`; `Transient` → next key; `Invalid` / `ToolsUnsupported` / `ContextOverflow` → immediate return without altering key status.
- SQL status updates are guarded: nothing modifies an `invalid` key, a success never clears a future cooldown, and a cooldown never shortens.
- Failed decryption logs `llm_usage` (`error_kind = "decrypt"`) and skips the key without changing status; if no key can be decrypted, returns `ChatError::Internal("cannot decrypt API keys — check ADUS_SECRET_KEY")`.
- `NoKeys` = nothing configured/usable; `AllKeysFailed { retry_after }` = all failed this call or all cooling down.
- Usage queries (`GET /api/v1/usage/daily`, `/usage/breakdown`) aggregate by UTC calendar day (inclusive `to`), zero-fill missing days with `generate_series`, include `cache_read_tokens` and `cache_write_tokens`, and label deleted FKs `(deleted)`. `POST /api/v1/ai-keys/{id}/test` uses `effective_fast_model()` with `TEST_KEY_MAX_TOKENS = 512`, `effort: None`, logs usage, and never changes key status.

---

## 7. Queue, Worker & Review Lifecycle (`crates/queue`, `crates/adus`)

- **Ingestion:** `POST /webhooks/github/{repo_id}` or `POST /webhooks/azure/{repo_id}?token=...` verifies signature/token, filters supported events, deduplicates delivery IDs in Redis (`SET NX`, 24 h TTL), enqueues `Job` onto `adus:jobs`, and returns `200`.
- **Execution (`crates/adus/src/worker.rs`):**
  - Per-PR distributed lock `pr:{repo_id}:{pr_number}` (`LOCK_TTL` = 35 min). Lock busy → `queue.retry` without incrementing attempt (30 s delay).
  - Jobs run inside `tokio::spawn` (`JOB_TIMEOUT` = 31 min backstop; panics return `Retryable("job panicked")`).
  - Compile-time assertions guarantee `REVIEW_TIMEOUT < SPECIALISTS_REVIEW_TIMEOUT < JOB_TIMEOUT < LOCK_TTL` and `JOB_TIMEOUT < RECLAIM_IDLE` (35 min) and `JOB_TIMEOUT < reviews::STALE_AFTER` (35 min).
  - `Retryable` → `queue.retry(attempt + 1, max(retry_after, backoff(attempt)))` while `attempt < MAX_ATTEMPTS` (5), else `dead_letter` (`adus:jobs:dead`) and `JobRunner::gave_up` (inserts one `failed` `reviews` row if repo still exists).
  - `Fatal` → `dead_letter` immediately.
  - Redis errors in the worker loop log and sleep 1 s. Heartbeat key `{prefix}:worker:{name}:hb` (`EX 30`) refreshes every 10 s.
  - Shutdown via shared `CancellationToken` gives in-flight jobs 30 s to drain.
- **Review row lifecycle:**
  - `Engine::review_pr` inserts one row (`queued` → `running` → `done | skipped | failed`).
  - On `Fatal`, marks the row `failed`. On `Retryable`, **deletes** the row so the retry creates a clean row.
  - On `adus serve` startup (and every `STALE_SWEEP_EVERY` = 5 min via `run_scheduler`), `reviews::fail_stale` marks `queued`/`running` rows older than `STALE_AFTER` (35 min) as `failed` (`"interrupted (no progress for 35 min)"`).

---

## 8. Agentic Review Engine (`crates/review`)

### 8.1 Triggers, Commands & Pre-Filters

- **Triggers:** PR `opened` (non-draft, `auto_review = true`), PR `updated` (incremental diff from last reviewed `head_sha` intersected with PR diff, or full if none), or `Trigger::Command`.
- **Commands (`crates/review/src/command.rs`):** Case-insensitive literal `@adudu` prefix from registered, non-disabled Adus users (Azure matches email; GitHub matches `users.github_login` or public email; unregistered authors ignored silently; comments containing `<!-- adudu -->` ignored):
  - `@adudu review` / `@adudu review generalist` / `@adudu review deep` (alias `specialists`)
  - `@adudu remember: <text>` (creates repo-scoped memory and replies with confirmation)
  - Unrecognized text after `@adudu` replies with command help.
- **Missing model:** If resolved provider (`repo.review_provider_id ?? settings.default_provider_id`) is missing, disabled, or has `primary_model = None`: `Opened`/`Command` post a "no model configured" comment and end `Fatal`; `Updated` ends `skipped` (`no_model`) silently.
- **Title keywords:** `settings.ignored_title_keywords` (case-insensitive substring, ≤ 100 chars each) → `skipped` (`ignored_title`), no comment. `Trigger::Command` bypasses.
- **Ignore paths (`adus_core::default_ignore`, `adus_db::ignore`):**
  - Deleted files are dropped first (never counted as ignored).
  - Remaining files matching `ignore_paths` (`globset` defaults on `normalize_path(new_path)`) are dropped before LLM work, excluded from `max_files_per_review`, and summarized as one `N files ignored` line (never listed by name). If all files are ignored → `skipped` (`nothing_to_review`).
  - Saved patterns are trimmed, leading `/` stripped, deduplicated, ≤ 500 chars each, ≤ 5,000 total, compiled on save (`Invalid`/`TooComplex` → 422 `invalid_glob`). Corrupted stored patterns fall back to defaults.
- **Size caps:** Files over `max_patch_lines_per_file` (1500) and PRs beyond `max_files_per_review` (30) are skipped with notes.

### 8.2 Small PR Fast Path (`crates/review/src/small_pr.rs`)

A non-command PR is reviewed **diff-only** (no clone, no tools, forced to `Generalist` with `ReasoningEffort::Medium`, Verifier diff-only, Dedup still runs) when **all** hold on the reviewed targets:
- `settings.small_pr_max_lines > 0` (default `100`) and total hunk `+`/`-` lines `<= small_pr_max_lines`
- `1 <= targets.len() <= SMALL_PR_MAX_FILES` (`3`)
- No `path` or `old_path` matches a risky path (`auth`/`security`/`crypto`/`permission`/`secret`/`token`/`session`/`password` segments, `migrations/`, `*.sql`, `schema` stem, `*.proto`, `openapi.*`, `*.graphql`, `lib.rs`, `mod.rs`, `index.ts`, `index.js`, `Dockerfile*`, `.github/workflows/**`, `Cargo.toml`, `package.json`)
- No target matches an enabled `critical` rule.
- Summary note: `"Small change — reviewed diff only"`, log `fallback_reason = "small_pr"`.

### 8.3 Ephemeral Clone, Read-Only Tools & Secret Redaction

- **Clone (`crates/review/src/workspace.rs`):**
  - Ephemeral shallow clone (`CLONE_TIMEOUT = 60 s`) under `<temp>/adus-clones/adus-{uuid}/` (0700 on Unix), cleaned on `Drop` (RAII) and swept at `adus serve` startup.
  - `GitCliCloner` clears env (forwarding only `PATH`, Windows basics, and proxy/CA vars), requires `git >= 2.31`, passes PAT only via `GIT_CONFIG_COUNT`/`GIT_CONFIG_KEY_0`/`GIT_CONFIG_VALUE_0` (`http.extraHeader`), disables hooks, symlinks, fsmonitor, LFS, submodules, `file://`, and enforces `MAX_WORKSPACE_BYTES = 500 MB`.
  - Auth error → marks Connection `invalid` and ends `Fatal`. Non-auth error / missing git < 2.31 / clone timeout → falls back to diff-only with summary note.
- **Tools (`crates/review/src/tools.rs`):**
  - `read_file` (200 lines / 8K chars, rejects binary & > 2 MB), `grep` (40 groups / 8K chars, 10 s timeout), `list_dir` (150 entries / 4K chars), `find_files` (150 paths / 4K chars), `submit_findings`, `submit_verdict`.
  - Paths validated through `RepoWorkspace::resolve` → `RepoPath` (blocks `..`, `.git`, symlink escapes, and secret files like `.env` [except `.env.example`/`.sample`/`.template`], `*.pem`, `*.key`, `id_rsa*`, `.npmrc`, etc.).
  - Shared per-review `ToolCache` (time-cut `grep`/`find_files` results are not cached).
- **Repo instructions (`crates/review/src/repo_instructions.rs`):** Reads `AGENTS.md`, `CLAUDE.md`, `.github/copilot-instructions.md`, `.cursor/rules/*.mdc` (capped at 8,000 chars total, wrapped in `<repository_instructions>` with close-tag neutralization).
- **Secret redaction (`crates/review/src/redact.rs`):**
  - Redacts (`[REDACTED]`) tool outputs before caching (`run_cached`), repo instructions before capping, and every posted inline comment + summary after Dedup (if a `suggestion` holds a secret, the suggestion fence is omitted with `"_Suggested code omitted: it contained a value that looks like a secret._"`).
  - Masks PEM key bodies, common provider token shapes, URL passwords, and secret-named assignments (env-style `KEY=value` only at line start, skipping code-shaped values and placeholders), preserving line numbers.
  - PR diffs and Verifier `<code_context>` snippets remain **unredacted** so hardcoded secrets in a PR are still detected and verified.

### 8.4 Pipeline Stages, Deadlines & Token Ceiling

- **Mode & Models:**
  - Effective mode: `command_override ?? repo.review_mode ?? settings.review_mode` (forced to `Generalist` on Small PR).
  - Run budget: `REVIEW_TIMEOUT` (`12 min`) for `Generalist` (incl. small PRs), `SPECIALISTS_REVIEW_TIMEOUT` (`30 min`) for `Specialists`.
  - Finder uses `provider.primary_model` (`Medium` effort for `Generalist`, `High` for `Specialists`). Verifier & Dedup use `provider.effective_fast_model()` (`Low` effort).
- **Stage Deadlines (from run start):**
  1. **Clone + Finder (`60 %`):** Diffs grouped into directory-clustered ~40K-char batches (`BATCH_CONCURRENCY = 2`, full PR manifest included). `Generalist` runs 1 agent/batch (20 steps, 2 wrap-up); `Specialists` runs `Bug`, `Security`, `Performance` + conditional `Rules` in parallel. Static prompt sections precede `<Role>` for prefix caching. `ToolsUnsupported` retries Finder once in diff-only mode.
  2. **Deterministic Filter & Exact Dedup:** Validates batch membership, RIGHT-side commentable line, `same_hunk` for `start_line` (drops `suggestion` if `start_line` is invalid), and deduplicates `(file, line, title)`.
  3. **Verifier (`90 %`):** Up to `MAX_VERIFY_CANDIDATES = 25` sorted `Critical > Warning > Info` (`VERIFY_CONCURRENCY = 4`, 6 steps, 1 wrap-up). Refute-to-drop, keep-by-default; **fail-closed** on error/timeout/unparseable. Switches remaining candidates to single-turn diff-only on `ToolsUnsupported` or `TokenBudget` exhaustion.
  4. **Semantic Dedup (`97 %`, last 3 % reserved for posting):** Single-turn call when `verified.len() >= 2`; safe fallback keeps `verified` unchanged on error/timeout.
- **ReAct Agent Loop (`crates/review/src/agent.rs`):**
  - Enters wrap-up (read-only tools dropped, only submit tool offered + 1 reminder) on last `wrap_up_steps`, when remaining stage deadline `< 2 × slowest_step`, or when `TokenBudget` is exhausted (after ≥ 1 step). Sends one halfway budget note when `max_steps >= 4`.
  - Compacts oldest tool results > 120 chars when prompt exceeds `160_000` chars; performs one-shot aggressive compaction on `ContextOverflow`.
- **Token Ceiling (`crates/review/src/budget.rs`):**
  - Shared `TokenBudget` (`settings.max_review_input_tokens`, `0` = off) counts `input_tokens` (incl. cached) of every successful LLM call across Finder, Verifier, and Dedup (soft cap: overshoot is in-flight calls + ≤ 1 wrap-up/agent + diff-only verifications).
  - Once exhausted: running agents wrap up, unstarted Finder batches are skipped (`token_budget`, non-retryable), unstarted Verifier candidates run diff-only, and Dedup still runs. Summary adds `"Token budget reached ({used} of {limit} input tokens): {details}."` only when the ceiling actually altered execution.
- **Failure & Retry Classification:**
  - `Retryable` (row deleted): all Finder tasks failed with `AllKeysFailed`/`NoKeys`/`Internal`; or all Verifier tasks failed retryably (`all_retryable_error`); or in `Generalist` mode, some Finder tasks failed and every failure was retryable. `Specialists` mode posts surviving findings with notes.
  - `Fatal`: deadline cut all Finder tasks (`review timed out`), all Finder tasks failed with `token_budget` as first non-retryable failure (`review stopped: token budget reached`), or unrecoverable error.
  - Posting errors: GitHub rate-limit/5xx/network → `Retryable`; GitHub other 4xx / all Azure post errors → `Fatal`.

---

## 9. Dashboard, CLI & Deployment

- **Dashboard pages (`web/`):** Login, Setup, Repositories (connections + repos), AI Providers (providers + keys), Review Rules (Rules & Memory tabs), Reviews, Token Usage, Users (admin), Settings (admin), System Status (admin doctor), Profile. See [DESIGN.md](./DESIGN.md) and [web/AGENTS.md](../web/AGENTS.md).
- **CLI subcommands (`crates/adus`):**
  - `adus serve` (default): sweeps orphaned clone workspaces + stale reviews, runs HTTP server + workers + scheduler.
  - `adus gen-secret`: prints 32 random base64 bytes.
  - `adus check-config`: offline env/URL validation.
  - `adus doctor`: reports `Reviews: OK | DEGRADED | NOT RUNNING` checking config, DB/migrations, Redis, worker heartbeats, default provider/model/keys, Connection PATs, repo webhooks, and `git >= 2.31` (warn if missing/old).
  - `adus healthcheck`: container health probe against `/healthz`.
- **Docker & Installers:** Multi-arch GHCR image `ghcr.io/return-nol/adus`. `docker-compose.yml` supports `bundled` (Postgres + Redis) and optional `https` (Caddy) profiles. `scripts/install.sh` and `scripts/install.ps1` support `--build` / `-Build` when prebuilt images cannot be pulled.

---

## 10. Historical Change Specs (Consolidated Archive)

| Date | Spec | Status |
|---|---|---|
| 2026-11-19 | [2026-11-19-adus-design.md](superpowers/specs/2026-11-19-adus-design.md) | Consolidated (v1 core platform) |
| 2026-11-20 | [2026-11-20-ignored-files-and-azure-project-design.md](superpowers/specs/2026-11-20-ignored-files-and-azure-project-design.md) | Consolidated (Azure `project_name`, ignore paths & title keywords) |
| 2026-11-21 | [2026-11-21-agentic-review-engine-design.md](superpowers/specs/2026-11-21-agentic-review-engine-design.md) | Consolidated (ephemeral clone, ReAct Finder/Verifier/Dedup, Small PR, token ceiling) |
| 2026-11-21 | [2026-11-21-provider-models-and-reasoning-design.md](superpowers/specs/2026-11-21-provider-models-and-reasoning-design.md) | Consolidated (`primary_model`/`fast_model`, model discovery, `ReasoningEffort`) |
