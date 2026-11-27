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
  - **Memory** is injected team context (only `active`, enabled memories reach prompts; `global` or `repo` scope), created in the dashboard or via `@adudu remember: ...` in a PR comment. No automatic conflict/dedup resolution.
- **PR discussion awareness & learned memories:** Semantic Dedup reads the PR's existing discussion threads and drops findings already raised there (§8.4). When a PR closes (merged or abandoned), one silent learning call turns "this is intentional because …" answers into repo-scoped memories (`source = pr_learning`, §8.5). Per repository `learning_mode`: `off`, `approval` (default; learned memories start `pending` until an admin approves or rejects them) or `auto` (`active` at once only when a registered Adus user defended the exception and every other non-Adus participant of the thread is a registered Adus user, else `pending`). Rejected memories are kept so they are never proposed again.
- **Clean-room policy:** Kodus non-EE code (outside `libs/ee/**` and `*.ee.*`) may be consulted as behavioral reference with citation, never ported line-by-line. Kodus Enterprise code (`libs/ee/**`, `*.ee.*`) must never be opened, fetched, or searched; EE-equivalent features are specified solely from public docs and observed behavior.

---

## 2. Architecture & Engineering Standards

### 2.1 Crate Workspace Layout

Dependencies remain strictly acyclic:

```text
crates/core     config, AES-256-GCM cipher (Cipher, last4), default ignore list, shared domain types & ReviewMode, Job payloads, normalize_path
crates/db       sqlx Postgres pool + embedded migrations (0001..0009), users, sessions, connections, repositories, ai, settings, usage, reviews, rules, memories, learning_runs
crates/git      GitProvider trait, GitProviderFactory, CloneSpec, diff utilities (commentable_lines, commentable_hunks, unified_patch), GitHub & Azure clients
crates/llm      LlmClient trait, OpenAI / Anthropic / Gemini / OpenAI-compatible clients, ReasoningEffort, LlmRouter (ChatService)
crates/queue    Redis Streams job queue (XADD / XREADGROUP / XAUTOCLAIM), delivery dedup, per-PR lock, worker heartbeats
crates/review   workspace (GitCliCloner, RepoWorkspace, RepoPath), tools, redact, repo_instructions, agent, coverage, specialists, finder, verifier, discussion, dedup, severity, enrich, budget, small_pr, prompt, parse, command, learn, engine
crates/server   axum REST API (/api/v1/*), webhook handlers (/webhooks/*), auth extractors, doctor checks, embedded SPA (rust-embed)
crates/adus     binary entrypoint (serve, gen-secret, check-config, doctor, healthcheck, hidden debug-only rereview), worker loop & scheduler
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

### 3.1 Database Schema (`migrations/0001` – `0009`)

- `users`: `id`, `email` (`citext`), `password_hash`, `role` (`admin | user`), `disabled`, `github_login` (`citext` nullable), `created_at`.
  - Always bind `citext` lookups as `$1::citext` and select as `::text`.
  - **Invariant:** At least one active `admin` must always exist (demote/disable guarded in a transaction with `FOR UPDATE`).
- `sessions`: `token_hash` (hex SHA-256 of 32-byte base64url cookie token), `user_id`, expiry (30 days).
  - Disabling a user or resetting their password deletes all their sessions; changing one's own password preserves the current session.
- `connections`: `id`, `kind` (`azure_devops | github`), `name`, `org_or_owner`, `base_url`, `encrypted_token`, `token_last4`, `status` (`ok | invalid | unverified`), `last_verified_at`, `pat_owner`.
- `repositories`: `id`, `connection_id`, `external_id`, `project` (Azure project GUID), `project_name` (Azure project display name, nullable, migration 0006), `name`, `default_branch`, `enabled`, `auto_review`, `webhook_external_id`, `webhook_secret`, `review_provider_id` (nullable), `review_mode` (nullable `review_mode`, migration 0008), `learning_mode` (`off | approval | auto`, `NOT NULL DEFAULT 'approval'`, migration 0009), `deleted_at` (soft-delete).
  - Duplicate active repos prevented via `ON CONFLICT` on a partial unique index.
  - **Display name invariant:** Computed server-side only via `repositories::label` (Rust) and `display_name_sql` (SQL): Azure `project_name/name` (fallback `owner/name` if `project_name` is `NULL`), GitHub `owner/name`. Never reconstructed in `web/`.
- `ai_providers`: `id`, `kind` (`openai | anthropic | gemini | openai_compatible`), `name`, `base_url`, `primary_model` (nullable in DB after migration 0008 = "needs setup", required on create), `fast_model` (nullable; `effective_fast_model()` falls back to `primary_model`), `enabled`.
- `ai_keys`: `id`, `provider_id`, `label`, `encrypted_key`, `key_last4`, `enabled`, `status` (`ok | invalid | rate_limited`), `cooldown_until`, `last_used_at`.
- `settings` (single row `id = 1`): `default_provider_id`, `ignore_paths` (`text[] NULL`, `NULL` = Kodus defaults, migration 0007), `ignored_title_keywords` (`text[] NOT NULL DEFAULT '{}'`), `review_mode` (`generalist | specialists`, default `generalist`), `max_files_per_review` (default `30`, `1..=500`), `max_patch_lines_per_file` (default `1500`, `1..=100000`), `small_pr_max_lines` (default `100`, `0..=10000`, `0` = off), `max_review_input_tokens` (default `2_000_000`, `0..=100_000_000`, `0` = no limit; counted weighted, prompt-cache reads at ¼ — §8.4).
- `rules`: `id`, `scope` (`global | repo`), `repository_id`, `title`, `body`, `path_glob`, `severity` (`info | warning | critical`), `enabled`.
- `memories`: `id`, `scope` (`global | repo`), `repository_id`, `content`, `source` (`manual | pr_comment | pr_learning`), `source_url`, `created_by`, `enabled`, `status` (`memory_status`: `pending | active | rejected`, `NOT NULL DEFAULT 'active'`, migration 0009; existing rows became `active`).
  - **Invariant:** Only `enabled` + `active` memories (`memories::applicable`) are injected into review prompts; `pending` and `rejected` never are. Manual and `@adudu remember` memories are created `active`.
- `pr_learning_runs` (migration 0009): `id`, `repository_id` (FK, `ON DELETE CASCADE`), `pr_number`, `head_sha`, `outcome` (text: `learned | nothing_to_learn | skipped | failed`), `learned_count`, `created_at`; `UNIQUE (repository_id, pr_number, head_sha)` so a PR head is learned at most once (`learning_runs::record` uses `ON CONFLICT DO NOTHING`).
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
  - `user` role: read-only access across pages + full management of `rules` and `active` `memories` + own password/profile + viewing token usage. `pending`/`rejected` memories can only be edited, deleted, approved or rejected by an admin (`403 forbidden` otherwise).
  - `admin` role: everything plus `connections`, `repositories`, `ai_providers`/`ai_keys`, `settings`, `users`, and `admin/doctor`.

### 4.2 API Contract & SPA

- Every `/api/v1/*` error returns `{"error":{"code":"<snake_case>","message":"..."}}` via `ApiError`.
- Handlers always use `ApiJson<T>`, `ApiPath<T>`, and `ApiQuery<T>` (never bare `axum::Json`, `Path`, or `Query`).
- `crates/server/src/spa.rs` embeds `web/build` via `rust-embed` (`#[allow_missing = true]`). Unknown `/api/*` and `/webhooks/*` routes return JSON 404; other `GET`/`HEAD` requests serve `index.html` with `Cache-Control: no-cache`; `_app/immutable/*` is served with `public, max-age=31536000, immutable`.
- Release/Docker builds must run `pnpm -C web build` before `cargo build --release`.

### 4.3 Memories & Learning Mode API

- `GET /api/v1/memories?repository_id=…` or `?all=true`, optional `&status=pending|active|rejected`: without `status`, every status except `rejected`; an unknown `status` → `422 validation_error`.
- `POST /api/v1/memories/{id}/approve` (admin): requires a JSON body (`{}` allowed; optional `content`, trimmed, ≤ 2,000 chars, replaces the text). `pending` → `active`.
- `POST /api/v1/memories/{id}/reject` (admin): has no body extractor (any or no body is accepted; the dashboard sends `{}`). `pending` → `rejected`.
- Both transitions are guarded in SQL (`WHERE status = 'pending'`): unknown id → `404 not_found`; a memory that is not `pending` → `409 invalid_status`.
- `PATCH /api/v1/repositories/{id}` (admin) accepts `learning_mode` (`off | approval | auto`; absent = unchanged); repository JSON includes `learning_mode`.

---

## 5. Git Providers & Webhooks (`crates/git`)

- Every HTTP request passes through a single `send` helper per provider (`github.rs`, `azure.rs`) that strips tokens/URLs from errors and enforces pagination capped at 30 pages (`tracing::warn!` on truncation).
- **Path normalization:** File paths are normalized without a leading `/` (`normalize_path`); a leading `/` is added only when calling Azure APIs.
- **GitHub:**
  - Maps `401` → `Unauthorized`; `429`, `403` with `x-ratelimit-remaining: 0`, or `403` with `Retry-After` → `RateLimited`; other unmapped errors → `Status(u16)`; network/timeout → `Http`.
  - Follows `Link` pagination only under its own base URL.
  - Webhooks: `pull_request`, `issue_comment`, `pull_request_review_comment`, verified via `X-Hub-Signature-256` HMAC. `ensure_webhooks` PATCHes an inactive or mismatched-URL hook (preserving its ID). `edited` webhook with a title change re-triggers a review. `pull_request` `closed` (merged or not) enqueues `Job::LearnPr`.
  - `CloneSpec`: `refs/pull/{pr.number}/head`, `Basic x-access-token:{pat}`.
- **Azure DevOps Services:**
  - `RepoRef.owner` = organization, `RepoRef.project` = project **GUID**, `RepoRef.external_id` = repo GUID. Every URL uses GUIDs.
  - `connectionData` is called **without** `api-version`. Maps both **203** (sign-in HTML) and **401** to `Unauthorized`.
  - Diffs are computed locally: file contents fetched via `items?$format=octetStream` and diffed via `similar` (`unified_patch`). A `404` on an expected side is an error (never an empty patch). `pr_diff` selects the iteration where `sourceRefCommit == pr.head_sha`, skipping `sourceRename`, folders, and submodules. Iteration changes report a deleted file with `item.path: null` and its path in `originalPath`; that path is used (an entry with neither is skipped). Decode errors carry serde's message (field and position, ≤ 200 chars).
  - Webhooks: one service hook subscription per event (`git.pullrequest.created`, `git.pullrequest.updated`, `ms.vss-code.git-pullrequest-comment-event`), authenticated via `?token={webhook_secret}` in the URL. Inactive or wrong-URL subscriptions are recreated. A PR created/updated event whose `resource.status` is `completed` or `abandoned` enqueues `Job::LearnPr` instead of a review.
  - `CloneSpec`: `pr.head_sha` with `fallback_refspec = pr.source_ref`, `Basic :{pat}`.
- **`list_pr_threads(repo, pr_number) -> Vec<PrThread>`:** A PR's discussion. `PrThread { path (normalized; None = top-level), line (new side; None when top-level/outdated), status, url, comments }`, `PrThreadComment { author, by_adus (body carries `<!-- adudu -->`), body }`, comments oldest first.
  - GitHub: review comments (`/pulls/{n}/comments`) grouped into threads by `in_reply_to_id` (a reply whose root is missing starts its own thread), then each issue comment and each non-empty review body as a one-comment top-level thread. Users with `type: "Bot"` are dropped (after grouping); deleted accounts appear as `ghost`; `status` is always `None`; `url` is the `html_url` only when http(s).
  - Azure: `/pullRequests/{n}/threads` (one request); system and deleted comments dropped, and comments without `author` or `author.uniqueName` (deprecated in IdentityRef) dropped, then threads left empty; `author` = `uniqueName` (email); `status` = Azure thread status (e.g. `byDesign`, `wontFix`); `line` = `rightFileStart.line`. `url` is always `None` (the threads API carries no web URL), so learned memories from Azure have `source_url` `NULL`.
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
  - `Openai` sends `max_completion_tokens`; `OpenaiCompatible` sends `max_tokens` and requires `base_url`. OpenAI-compatible tool calls lacking an `id` receive `call_{prior}_{index}`. OpenAI-compatible responses capture non-blank `reasoning_content` / `reasoning` and non-empty `reasoning_details` arrays into `ChatResponse.provider_meta` (`has_reasoning = provider_meta.is_some()`, so `reasoning_details`-only responses also trigger `"empty response: model exhausted max_tokens during reasoning"` when `finish_reason == "length"` with empty text and tool calls), and `assistant_message` replays only allowlisted keys (`["reasoning_content", "reasoning_details", "reasoning"]`) from `AssistantPart::ProviderMeta` on later assistant turns (ignored by `Anthropic` and `Gemini`).
  - `Anthropic` places `cache_control: {"type": "ephemeral"}` on system blocks, the last tool definition, and the last block of the latest user/tool message (≤ 4 breakpoints), maps `Message::Tool` to `"role": "user"`, preserves `Thinking`/`RedactedThinking` blocks in `provider_meta`, and computes `input_tokens = input + cache_read + cache_write` so usage matches OpenAI/Gemini. `adaptive_thinking` splits the id after `claude-` on `-` or `.` (non-`claude-` and `claude-3*` keep budget mode; `opus`/`sonnet`/`haiku` ≤ 4.5 with 1–2 digit minor keep `thinking: {"type": "enabled", "budget_tokens": ...}`; 4.6+ and other `claude-*` ids send `thinking: {"type": "adaptive", "display": "summarized"}` and `output_config: {"effort": ...}`). `"display": "summarized"` is permanent: it changes only what the response shows (thinking is billed the same), and without it Claude 4.7+ / 5.x return empty thinking blocks, so the live trace (§8.4) would show no reasoning.
  - `Gemini` maps `Message::Assistant` to `"role": "model"`, preserves `thoughtSignature` in `provider_meta`, ignores `part.thought == true` when building response text, and assigns deterministic IDs `gc_{prior}_{idx}`. `thinking_level` parses the full multi-digit integer immediately after `gemini-` (stopping at `.` or `-`) and sends `thinkingConfig.thinkingLevel` (`LOW` / `MEDIUM` / `HIGH`, with `gemini-3-pro-preview` mapping `Medium` → `HIGH`) for Gemini 3+/4+, or `thinkingConfig.thinkingBudget` for older/unversioned ids.
  - **OpenRouter** (supersedes the OpenRouter part of the 2026-11-24 reasoning spec): `OpenAiClient` (both `Openai` and `OpenaiCompatible`) detects OpenRouter when `base_url` parses with host `openrouter.ai` or `*.openrouter.ai` (case-insensitive). With an effort it sends `"reasoning": {"effort": ...}` and `"provider": {"require_parameters": true}` instead of bare `reasoning_effort`, so OpenRouter routes only to upstreams that honour every parameter rather than silently dropping reasoning (or tools). Any `Invalid` error (e.g. a 404, not status-gated) whose lowercase message contains `"no endpoints found"` (a model that cannot honour `require_parameters`) is retried once with `reasoning` but without `provider.require_parameters`; only if that also fails does the generic effort fallback below apply (`effort: None`). Every other OpenAI-compatible endpoint, DeepSeek direct included, keeps bare `reasoning_effort`. The output token reserve is added to the cap on OpenRouter too.
- **Reasoning effort (`ReasoningEffort::{Low, Medium, High}`):**
  - Adds an output token reserve (`Low: 4,096`, `Medium: 12,000`, `High: 23,000` — also used as the Gemini 2.5 / unversioned Gemini `thinkingBudget` and legacy Claude `budget_tokens`) and raises request timeout to 300 s.
  - If a non-404 `Invalid` matches `REASONING_UNSUPPORTED_MARKERS` (including `"output_config"`) or names `max_tokens` / `max_completion_tokens` / `maxOutputTokens` alongside an excess/range cue (cap-overflow fallback while `effort.is_some()`), `chat` transparently retries once with `effort: None` (restoring `req.max_tokens` without reserve, 120 s timeout).
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

- **Ingestion:** `POST /webhooks/github/{repo_id}` or `POST /webhooks/azure/{repo_id}?token=...` verifies signature/token, filters supported events, deduplicates delivery IDs in Redis (`SET NX`, 24 h TTL), enqueues `Job` onto `adus:jobs`, and returns `200`. Job kinds (`adus_core::job::Job`, serde tag `type`): `review_pr` (`repository_id`, `pr_number`, `trigger`, `mode_override`), `pr_comment` (`@adudu` command candidate), `learn_pr` (`repository_id`, `pr_number`; runs `Engine::learn_pr`, §8.5).
- **Execution (`crates/adus/src/worker.rs`):**
  - Per-PR distributed lock `pr:{repo_id}:{pr_number}` (`LOCK_TTL` = 35 min) for every job kind. Lock busy → `queue.retry` without incrementing attempt (30 s delay).
  - Jobs run inside `tokio::spawn` (`JOB_TIMEOUT` = 31 min backstop; panics return `Retryable("job panicked")`).
  - The engine bounds a review by its own budget so its `reviews` row is recorded as failed: `REVIEW_TIMEOUT` = **20 min** (`Generalist`, incl. small PRs) and `SPECIALISTS_REVIEW_TIMEOUT` = **30 min** (`Specialists`). Stage deadlines are fractions of that budget counted from the run's start (the PR fetches and the clone included): `Deadlines { finder: 55 %, verifier: 80 %, post_verify: 93 % }` (§8.4), i.e. 11 / 16 / 18.6 min of 20 (16.5 / 24 / 27.9 min of 30); the last 7 % is for formatting and posting. Post-verify Enrichment stops starting calls when less than `ENRICH_RESERVE` = 20 s is left to `post_verify`.
  - Compile-time assertions guarantee `REVIEW_TIMEOUT < SPECIALISTS_REVIEW_TIMEOUT < JOB_TIMEOUT < LOCK_TTL` and `JOB_TIMEOUT < RECLAIM_IDLE` (35 min) and `JOB_TIMEOUT < reviews::STALE_AFTER` (35 min).
  - `Retryable` → `queue.retry(attempt + 1, max(retry_after, backoff(attempt)))` while `attempt < MAX_ATTEMPTS` (5), else `dead_letter` (`adus:jobs:dead`) and `JobRunner::gave_up` (for `review_pr` only: inserts one `failed` `reviews` row if repo still exists; `pr_comment` and `learn_pr` record nothing).
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

A non-command PR is reviewed **diff-only** (no clone, no tools, forced to `Generalist` at the same `High` Finder effort as any review, one pass with no Synthesis Rescue, no coverage ledger and no `<changed_functions>`; the Verifier is a single diff-only call with no Evidence Gate; Dedup, Severity and Enrichment still run, Enrichment without file context lines) when **all** hold on the reviewed targets:
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
  - `read_file` (200 lines / 8K chars, rejects binary & > 2 MB; a successful result starts with a `[path: lines X-Y of Z]` header, `0-0` for an empty file), `grep` (40 groups / 8K chars, 10 s timeout), `list_dir` (150 entries / 4K chars), `find_files` (150 paths / 4K chars), `submit_findings`, `submit_verdict`.
  - Paths validated through `RepoWorkspace::resolve` → `RepoPath` (blocks `..`, `.git`, symlink escapes, and secret files like `.env` [except `.env.example`/`.sample`/`.template`], `*.pem`, `*.key`, `id_rsa*`, `.npmrc`, etc.).
  - Shared per-review `ToolCache` (time-cut `grep`/`find_files` results are not cached).
  - `Toolbox::read_file_range` resolves a `read_file` call to `(normalized path, start, end)` (through `RepoWorkspace::resolve_existing_file`); the agent loop credits a `CoverageLedger` only with the range shown in the result header (§8.4).
  - **Verifier auto-widen:** with `AgentConfig::widen_short_reads` (Verifier runs only), a `read_file` call with explicit `start_line` and `end_line` where `end_line >= start_line` and `end_line - start_line < 20` is widened to `start_line - 20` (at least 1) ..= `end_line + 10` before it reaches the `Toolbox` (still capped by the 200-line / 8K-char limits), so enclosing guards and error handlers show. Finder runs are never widened.
  - **Raw hunks only:** `prompt::expand_diff_block` and `tools::load_expanded_diffs` no longer exist; Finder prompts always carry the raw annotated hunks, and the model opens surrounding code with `read_file`.
- **Repo instructions (`crates/review/src/repo_instructions.rs`):** Reads `AGENTS.md`, `CLAUDE.md`, `.github/copilot-instructions.md`, `.cursor/rules/*.mdc` (capped at 8,000 chars total, wrapped in `<repository_instructions>` with close-tag neutralization).
- **Secret redaction (`crates/review/src/redact.rs`):**
  - Redacts (`[REDACTED]`) tool outputs before caching (`run_cached`), repo instructions before capping, and every posted inline comment + summary after the post-verify funnel (if a `suggestion` holds a secret or an already-masked `[REDACTED]` value, the suggestion fence is omitted with `"_Suggested code omitted: it contained a value that looks like a secret._"`).
  - Masks PEM key bodies, common provider token shapes, URL passwords, and secret-named assignments (env-style `KEY=value` only at line start, skipping code-shaped values and placeholders), preserving line numbers.
  - PR diffs (raw hunk `+`, `-`, ` ` lines from `file.patch`) and Verifier `<code_context>` snippets remain **unredacted** so hardcoded secrets in a PR are still detected and verified. The Enrichment `<code_context>` (§8.4) is redacted, and so is every recent-tool-call summary shown to the Synthesis Rescue and every logged `reasoning` / dismissal / trace line.

### 8.4 Pipeline Stages, Deadlines & Token Ceiling

- **Mode & Models:**
  - Effective mode: `command_override ?? repo.review_mode ?? settings.review_mode` (forced to `Generalist` on Small PR).
  - Run budget: `REVIEW_TIMEOUT` (`20 min`) for `Generalist` (incl. small PRs), `SPECIALISTS_REVIEW_TIMEOUT` (`30 min`) for `Specialists`.
  - Two model slots, with these efforts (an agent step asks for `8,192` output tokens before the client adds the effort's reserve, §6.1):

    | Stage | Slot | Effort | Output cap |
    |---|---|---|---|
    | Finder Pass 1 and Pass 2 (every role, both modes, Small PR) | `primary_model` | `High` (`FINDER_EFFORT`) | agent step |
    | Verifier: light run, Evidence Gate, diff-only call | `primary_model` | `High` (`VERIFY_EFFORT`, the Finder's own) | agent step; `2,048` diff-only |
    | Prose Recovery | `effective_fast_model()` | `Medium` | `2,048` |
    | Semantic Dedup | `effective_fast_model()` | `Medium` | `1,024` |
    | Severity Classifier | `effective_fast_model()` | `Medium` | `1,024` |
    | Enrichment | `effective_fast_model()` | `Medium` | `2,048` |

    (`learn_pr` extraction, §8.5, is outside the review pipeline and unchanged.)
- **Stage Deadlines (from run start, fractions of the run's budget):**
  1. **Clone + Finder (`55 %`, 11 min of 20, 16.5 min of 30):** The clone counts against this deadline; a clone still running at it falls back to diff-only. Diffs are grouped into directory-clustered ~40K-char (`MAX_BATCH_CHARS`) batches (`BATCH_CONCURRENCY = 2`, full PR manifest in every prompt). `Generalist` runs 1 agent per batch; `Specialists` runs `Bug`, `Security`, `Performance`, plus `Rules` (`active_specialists(mode, !batch.rules.is_empty() || input.repo_instructions.is_some())`) in parallel. Details in **Finder** below.
  2. **Deterministic Filter & Exact Dedup:** Validates batch membership, RIGHT-side commentable line, `same_hunk` for `start_line` (drops `suggestion` if `start_line` is invalid), and deduplicates `(file, line, title)`; lean Finder findings have no title yet, so the body stands in for an empty title and only findings that say the same thing merge. `collect_candidates` counts `filtered_dropped` across `keep_valid_batch` before `dedup_exact` and logs it in `review stages finished`. Every candidate records its `origin` and whether the Finder opened its file (`investigated_by_finder`, from the task's `opened_files`).
  3. **Verifier (`80 %`, 16 min of 20):** see **Verifier** below.
  4. **Post-verify funnel (`93 %`, 18.6 min of 20, `Deadlines::post_verify`):** Semantic Dedup, then Severity, then Enrichment ending with `check_fix`, see **Post-verify funnel** below. The remaining 7 % is for formatting and posting.
- **Finder (`finder.rs`, `prompt.rs`, `specialists.rs`, `parse.rs`):**
  - **Prompt.** The user prompt carries `<pr_title>`, `<pr_description>`, `<pr_manifest>`, `<rules>`, `<memories>`, `<repository_instructions>` (when loaded), `<changed_functions>` (tool-enabled runs, when any hunk header names a scope) and `<diffs>`. Diffs are always the **raw annotated hunks** (`=== path (status) ===`, each `+` and ` ` line prefixed with its new-file line number); the model must open surrounding code with `read_file`. `<pr_description>` is cut to `MAX_DESCRIPTION_CHARS_TOOLS = 500` chars for tool-enabled runs (`MAX_DESCRIPTION_CHARS_DIFF_ONLY = 4_000` for diff-only runs and the Verifier), marked `[truncated]`. `<changed_functions>` lists per file, one `- path: name, name` line, the enclosing-scope text git writes after each hunk header (`@@ -a,b +c,d @@ <context>`), cut before the first `(` or `{`, deduplicated per file in hunk order, at most `CHANGED_FUNCTIONS_MAX = 30` names over all files; a hint only (no AST), possibly empty for some languages or providers.
  - **System prompt.** Static `<ReviewInstructions>` precede `<Role>` (role-specific playbooks via `SpecialistKind::role_block_for(with_tools)`, using `BUG_ROLE_DIFF_ONLY` when `!with_tools` so `Bug` diff-only prompts stay free of tool names) for byte-identical prefix caching across specialists. The instructions state a real-team-PR, adversarial stance ("treat every change as unproven until you verify why it cannot fail; 'looks correct' is not a valid dismissal") and a 4-step Workflow: (1) the **first action must be a tool call**, not prose or `submit_findings`, and every changed region is opened with `read_file`; (2) trace its reach by grepping the callers of each `<changed_functions>` entry (and of each changed hunk without one), up to 4 parallel read-only calls per turn; (3) challenge each change through every lens the Role names, where a code-backed suspicion anchored to a `+` line is promoted without full proof (the Verifier filters) and a suspicion is dismissed only by naming the guard, type, constraint or caller read; (4) finish once `reasoning` accounts for every changed function and every lens. No sentence tells the model to submit early or to spend fewer steps. `<Scope>` keeps the rules: findings only on `+` lines (cross-file breakage on the changed `+` line that causes it); `<pr_description>` states the goal, not correctness, so side effects it does not cover are in scope; older defects count when the change makes them worse or newly reachable; findings are written as failures with their trigger, never as questions; the do-not-report list is limited to hypotheticals prevented by guards the agent read and unrequested style. The `Generalist` Role requires all three lenses with concrete checklists (Bug: swallowed errors, errors treated as a cache miss or not-found, dropped fallbacks, SQL `NULL` propagation, wrong conditions, races/TOCTOU, migration hazards, wrapper/delegation bugs; Security: untrusted input to sinks, authn/authz that fails open, path traversal, SSRF, unsafe deserialization, secrets; Performance: queries or calls in loops, unbounded queries/loops, blocking calls on hot paths), then the matching `<rules>` and `<repository_instructions>`.
  - **Steps.** `finder_max_steps(files) = (FINDER_BASE_STEPS 20 + files.saturating_sub(FINDER_FREE_FILES 8).div_ceil(2)).min(FINDER_STEP_CAP 30)`, `FINDER_WRAP_UP_STEPS = 2`. Every agent also wraps up when the time to the Finder deadline is `< 2 ×` its slowest step.
  - **`submit_findings` (lean schema).** `reasoning` (**required**, listed before `findings` in the schema and the prompt; `serde_json` is built with `preserve_order` so the property order survives on the wire): for each changed function or hunk, what was challenged, which callers or callees were read, and why each suspicion was reported or dismissed. A missing or blank `reasoning` is logged (`finder reasoning missing`, debug) but never refused, so a non-compliant model loses no findings; it is secret-redacted, logged at debug (`finder reasoning`) and never posted. `findings` items: `file`, optional `start_line`, `line` (a `+` line), `category` (`bug | security | performance | repo_instruction`; required from the `Generalist`, defaulted from the `SpecialistKind` otherwise, accepted either way) and `body` (the concrete trigger, impact and code path, then the fix; posted as written after verification). `title`, `severity` and `suggestion` are **no longer requested** from the Finder (legacy JSON carrying them still parses); the post-verify funnel sets them. When `findings` (or `dismissed`) arrives as a JSON-encoded string instead of an array, the string is parsed and used if it is a JSON array; any other string is rejected. `submit_findings` validates batch paths and commentable `+` lines (`parse::validate_batch_anchors` via `SubmitSpec::parse_arguments(..., can_retry)`), bouncing once with valid files or up to 5 nearest `+` lines while a step remains (`anchor_retried`). Tool-enabled Finders may also pass an optional `dismissed` list (`file`, optional `line`, `suspicion`, `why_safe`): `parse::parse_dismissed` keeps at most `MAX_DISMISSED = 30` entries and secret-redacts every text (`file` included) before cutting it to `DISMISSED_TEXT_CHARS = 300` chars, and `SubmitFindings` logs them at debug level (`"finder dismissed"` per entry, `"finder dismissals"` count, with `specialist` and `batch`) on the accepted call only; dismissals are never posted, persisted or shown to the Verifier. Diff-only Finder prompts never ask for dismissals (their output format is the plain JSON array, though they may still call the offered `submit_findings` with `reasoning` and `findings`). `ToolsUnsupported` re-runs every Finder agent once in diff-only mode (the Verifier then works diff-only too).
  - **Per-hunk `CoverageLedger` (`coverage.rs`).** Only the gated runs, `Generalist` and `Bug` **Pass 1 with the repository tools**, carry a ledger (and so debt notes and the gate). `Security`, `Performance`, `Rules`, Pass 2, the Verifier and diff-only runs get neither: a narrow lens may rightly ignore a file. `CoverageLedger::from_files` keeps one target per batch file that is **not `Deleted`** (no head version to open) and has at least one commentable right-side hunk (`adus_git::diff::commentable_hunks`), so binary, rename-only and mode-only files carry no debt. A hunk counts as read only when its whole `[start, end]` lies inside one merged window of lines the model saw. Only successful (`!is_error`) `read_file` results whose `[path: lines X-Y of Z]` header parses credit the ledger, with the **header's** range (so a read capped at 200 lines / 8K chars credits only what was shown; `0-0`, an empty file, covers every hunk). `grep`, `list_dir`, `find_files`, failed reads, reads of files outside the batch and reads issued in the same turn as a submission do not count. Only qualifying files carry debt: not a doc file (`GATE_DOC_EXTENSIONS = ["md", "mdx", "rst", "txt"]`) and not a test path (`*_test.*`, `*.test.*`, `*.spec.*`, `tests/**`, `__tests__/**`); the note and the gate use the same set.
  - **Debt note (append-only).** Before a step with repository tools, outside wrap-up, when a qualifying file has pending hunks and the pending set differs from the one named by the last note of this run (never on the first request), a user turn is appended after the step's tool results and **kept in the conversation**; nothing is injected for one request only and dropped later, so the prompt-cache prefix stays valid and providers that check preserved thinking (Sonnet 5.5, Opus 5.5) accept the history. It lists up to `COVERAGE_DEBT_MAX_FILES = 8` files and `COVERAGE_DEBT_MAX_RANGES = 3` ranges per file (more ends with `, …`):

    ```text
    Unread changed code. You have not opened these changed lines with read_file yet:
    - path/to/file.rs: lines 40-55, 80-95
    Read them before you call submit_findings. A grep or list_dir match does not count as reading them.
    ```
  - **One-shot completion gate.** On the first parsed submission (`submit_findings` call or prose JSON) of a gated run, when (1) the gate has not refused before, (2) the run is not wrapping up, (3) at least `GATE_MIN_STEPS_LEFT = 2` investigating steps remain after the current one (`max_steps - max(wrap_up_steps, 1) - steps_used`, the current step included in `steps_used`) and (4) a qualifying file still has pending hunks, the gate refuses **once** with a non-error `ToolResult` (a user message for prose submissions), and refuses any second `submit_findings` of the same turn with the same text: `Not submitted yet. Before you submit, open these changed files with read_file and check the code around each change: {files}. Then call submit_findings again with every finding you confirmed. This is asked only once; your next submit_findings call will be accepted.` The refused submission is held; if the run later ends unparseable the held submission is returned (`cut_short`, `gate_refused`). **Empty resubmission keeps earlier findings:** an accepted submission without findings takes the findings of the latest refused one that had some (its `reasoning` and `dismissed` are kept), because a model refused once often resubmits `[]` meaning "already sent". The refusal is at most once per run, so it can make a model that skipped the files open them but never traps it.
  - **Budget bands (Finder only).** With `AgentConfig::budget_bands` (set by every Finder pass; Verifier and other runs leave it off because the texts speak of findings and dismissals), repository tools, outside wrap-up and `max_steps >= 6` (`BUDGET_BANDS_MIN_STEPS`): `force_start = max_steps - max(wrap_up_steps, 1)`, `urgent_from = max(force_start - 3, 3)`, `encourage_from = max(urgent_from - 4, 2)`. Each band's note is appended **once** on entering `Encourage` or `Urgent` and kept in the conversation. Texts (`{submit}` = `submit_findings`):
    - `Encourage`: `Step budget: {used} of {max} steps used. Keep going through the changed code you have not checked yet, and batch independent lookups in one turn.`
    - `Urgent`: `Step budget: {left} steps left before only {submit} is offered. Finish the check you are on, then call {submit} with every finding and dismissal so far.`
  - **Pass 2 Synthesis Rescue (`synthesis-rescue`).** After Pass 1 (and its Prose Recovery), `Generalist` and `Bug` agents with the repository tools run a second pass for **every model** when both gates hold; `Security`, `Performance` and `Rules` run Pass 1 only. (1) **Budget:** at least `SYNTHESIS_MIN_BUDGET_SHARE = 25 %` of the weighted `max_review_input_tokens` is left (always true without a limit). (2) **Time:** the time to `deadlines.finder` is at least `SYNTHESIS_MIN_WINDOW = max(SYNTHESIS_WINDOW_STEPS 6 × Pass 1 mean LLM step, 60 s)` (`SYNTHESIS_DEFAULT_STEP = 90 s` when Pass 1 measured no step). A skipped Pass 2 sets the stage log's `synthesis_skipped = "budget" | "time"`. Pass 2 reuses the same `Toolbox` and `ToolCache`, runs at most `finder_max_steps(files).min(SYNTHESIS_MAX_STEPS = 12)` steps with the same wrap-up and bands but **no** ledger, debt note or gate, and sends Pass 1's prompt unchanged (cache prefix) followed by `<already_inspected_files>` (sorted files Pass 1 opened), `<recent_investigation>` (the last `RECENT_TOOL_CALLS = 20` read-only calls, `tool(arguments)`, secret-redacted, arguments cut to `TOOL_CALL_ARGS_CHARS = 150`), `<current_findings>` (`- file:line: body` cut to `FINDING_EXCERPT_CHARS = 200`) and a task asking for concrete **missed** defects (error and failure paths, what callers pass and expect, edge cases, branches not followed), never a repeat or rewording of a current finding, and `[]` when none. New findings are merged by `(normalized file, line, first MERGE_BODY_PREFIX_CHARS = 80 chars of the lowercase whitespace-normalized body)`; a failed Pass 2 keeps Pass 1's findings. `investigated_by_finder` is the union of the files either pass opened. Every candidate carries `origin: FinderPass` (`Pass1 | SynthesisRescue | ProseRecovery`), logged on the Verifier's per-candidate `verifier verdict` debug line and counted per origin as `kept_by_origin` in the stage log.
  - **Prose Recovery.** After either pass, when `findings` is empty but the pass's prose (its `reasoning`, or the reply text of a prose submission; secret-redacted) is at least `PROSE_MIN_CHARS = 80` chars and matches at least `PROSE_MIN_SIGNAL_GROUPS = 2` of three signal groups (defect-word stems, action words such as `should`/`must`/`fix`, a `path.ext:12` or `line 12` reference), one call on the fast model at `Medium` (`2,048` output tokens, cut at the Finder deadline) turns it into lean findings; only findings on a batch file with a stated line are kept, with `origin = ProseRecovery`. Any failure recovers nothing.
- **Verifier (`80 %`, `verifier.rs`):**
  - **Model & steps.** `primary_model` at `High`. With a `Toolbox` every candidate gets a **light** tool run of `VERIFY_LIGHT_MAX_STEPS = 7` steps (1 wrap-up); without one (Small PR, failed clone, `ToolsUnsupported`) or once the token budget is exhausted when the candidate starts, a single diff-only call (`VERDICT_MAX_TOKENS = 2,048`, also `High`; counted as a diff-only verification). `AgentConfig { no_tools_on_last_step: true, widen_short_reads: true }`: on the final step (`steps_used + 1 == max_steps`) **no tool at all** is offered (not `submit_verdict` either) and the model is told to reply with only the final JSON verdict (`{"keep": true|false, "rationale": "..."}`); one step earlier a **verdict-due note** is appended once (append-only like the debt note): `One step left for reading. After it you must answer with your final JSON verdict, without tools, based on what you have read.` Earlier steps offer the read-only tools plus `submit_verdict`. The Verifier gets no mid-run step-budget note (bands are Finder-only) and no gate. Short `read_file` calls are auto-widened (§8.3).
  - **Prompt.** `verifier::build_user_prompt` prepends `<pr_title>`, `<pr_description>` (4,000-char cap, shared `prompt::format_pr_description`), and `<pr_manifest>` in the shared cacheable prefix ahead of `<rules>`, `<memories>`, `<repository_instructions>`, `<diff>`, `<code_context>` (about ±`SNIPPET_RADIUS = 40` lines around the span, unredacted) and `<candidate>` (`VERIFIER_UNTRUSTED_INPUT` lists all of them; `VERIFIER_WORKFLOW_TOOLS` notes up to 4 parallel tool calls per turn). Refute-to-drop, keep-by-default, with explicit `<Verdict>` drop rules for `<memories>` exceptions, behaviour the repository itself states as intended (a code comment, doc beside the code, or pinning test — never for a security hole, data loss or crash), and speculative hedges; `<pr_description>` is context only, never by itself grounds to drop, and edge-case triggers or older code the change makes reachable are not grounds to drop; findings reported by the `Rules` reviewer or citing a team rule or repository instruction are never dropped as style (only when the rule does not apply to the file or the changed line does not break it); optional `drop_suggestion: bool` on `submit_verdict` (`Verdict`) clears `suggestion` and `start_line` while keeping the finding when a replacement patch is broken or incomplete.
  - **Verdict semantics.** **Dropped as unverified** (`dropped_unverified`): a provider error, a deadline cut, or a run that ends without ever producing a reply to parse. **Kept when the verdict cannot be read:** a run that reaches its final reply is first recovered, from a `submit_verdict` call with loose arguments, then from a JSON object (or a bare `"keep": true|false`) in the reply text; if nothing usable is found the finding is **kept** (`verifier verdict unreadable, kept`), because failing to state a refutation is not one. A candidate that starts after the Verifier deadline is dropped as unverified.
  - **Evidence Gate.** After the light pass, every finding kept by a **tool-enabled** light run whose file the Finder never opened (`!investigated_by_finder`) is re-verified by a second tool run of `VERIFY_FULL_MAX_STEPS = 10` steps sharing the `Toolbox` and `ToolCache`. A gate verdict, when the run produces one, supersedes the light verdict. A gate run that ends without a verdict (deadline cut, provider error, no final reply) **keeps the light verdict**, since gate runs start last and the deadline would otherwise cut exactly the findings the gate exists to strengthen. A light verdict from a single diff-only call (budget exhausted, or tools rejected) is final, and no gate run starts once the token budget is exhausted or tools are off, since it could only repeat that call. `evidence_gate_runs` counts the gate tool runs that finished before the deadline.
  - **No candidate cap.** There is no `MAX_VERIFY_CANDIDATES` cap: every candidate is queued, `VERIFY_CONCURRENCY = 4` at a time, bounded only by the Verifier deadline and the `TokenBudget` (a candidate that starts after the deadline is dropped as unverified; one that starts after the budget ran out gets the diff-only call). Because those two cut the end of the queue, `verify_order` starts candidates by a stable sort on (1) **category** `security` → `bug` → `performance` → `repo_instruction` (a candidate without a `category` takes it from its `SpecialistKind`: `Security` → `security`, `Performance` → `performance`, `Rules` → `repo_instruction`, `Bug` and `Generalist` → `bug`), (2) **origin** `Pass1` → `ProseRecovery` → `SynthesisRescue`, (3) original position. The Evidence Gate queue uses the same order. `verify_candidates` returns the kept findings in their original order; the `"{n} candidate finding(s) could not be verified and were omitted"` warning reports `dropped_unverified`.
- **Post-verify funnel (`dedup.rs`, `severity.rs`, `enrich.rs`):** all three stages are bounded by `deadlines.post_verify`; a stage cut by it falls back as described, and none ever drops a finding for its own failure.
  1. **Semantic Dedup (`dedup.rs`):** Single-turn call (fast model, `Medium` effort, ≤ 1,024 output tokens) that merges duplicates (`keep_indices`, preferring (1) higher severity when findings carry one, (2) concrete file/symbol evidence and tighter line anchor, and (3) rule/instruction citations; an empty list counts as "keep all" only when `already_raised` has at least one valid citation — finding and thread both exist — otherwise it is invalid) and drops findings an existing PR thread already raised (`already_raised: [{finding, comment}]`). No call when there are no findings, or one finding and no existing thread. Threads come from one `list_pr_threads` per review (only when there are files to review; on error the review continues without them, logged). `discussion::threads_for_dedup` picks threads on a finding's file, then top-level threads, each newest first (reverse provider order), at most 60, then cut so the formatted threads stay within 40,000 chars (`discussion::within_budget`: the first thread that would exceed it and all later ones are dropped); bodies are secret-redacted and cut to 1,000 chars, Adus's comments are labelled author `"adus"`, and the block is wrapped as `<existing_comments>` data. A citation of a nonexistent thread is ignored (the finding stays); the cited thread is not checked against the finding's file. Safe fallback keeps `verified` unchanged on error/timeout/unparseable answer/invalid `keep_indices`. When `already_raised > 0` the summary adds `"{N} finding(s) skipped: already raised in this PR's discussion."`. Candidates keep their `origin` and category through Dedup (`kept_by_origin` is counted after it).
  2. **Severity (`severity.rs`):** one single-turn call (fast model, `Medium`, ≤ 1,024 output tokens) labels every finding with `severity = None` as `critical`, `warning` or `info` from its location, category and body (rating only what the body states); findings that already carry a severity are neither sent nor changed, and no call is made when none is missing. A finding the answer does not label (LLM error, unparseable answer, missing or unknown entry, or the stage cut by the deadline) becomes `Warning`; nothing is dropped. All verified findings are posted (no severity filter).
  3. **Enrichment (`enrich.rs`):** one call **per finding** (`ENRICH_CONCURRENCY = 4`; fast model, `Medium`, ≤ 2,048 output tokens) with `<finding>` (file, `start_line`, `line`, category, body), `<diff_hunk>` (the annotated hunk around `line`) and, with a `Toolbox`, `<code_context>` (about ±40 lines, **secret-redacted**). Answer `{"title", "start_line"?, "suggestion"?}`: a non-blank title (one line, ≤ 100 chars asked) replaces the finding's title, and a suggestion replaces the finding's suggestion and span (`start_line` from the answer, none meaning `line` alone). **The verified `body` is never rewritten** (any `body` in the answer is ignored): the Verifier checked the Finder's body, and a rewrite could add claims nobody checked; the posted comment body is the Finder's `body`, secret-redacted. No call starts when less than `ENRICH_RESERVE = 20 s` is left to `post_verify`, and a call still running at it is cut. A finding left without a title (not started, LLM error, unreadable answer, blank title) gets the first sentence of its body's first line, at most 100 chars (a longer one is cut and ends with `…`), and keeps no suggestion. Enrichment still makes one call per finding when the token budget is exhausted (as Dedup and Severity do): the calls are small and their tokens are still counted.
  4. **`check_fix` (end of `enrich_suggestions`):** every `suggestion` is validated; one that fails loses `suggestion` and `start_line` and the finding is posted as a plain comment (the finding is never dropped). It fails when it is empty or whitespace-only; contains a markdown fence (```` ``` ````); is identical to the lines it replaces (trailing whitespace and trailing blank lines ignored); has a `start_line` that does not lie in one hunk with `line` of the file's `CommentableRange`; or its net `()`, `[]`, `{}` balance (outside string and char literals) differs from that of the replaced lines. The replaced lines are read from the diff's right side (context and `+` lines) for `start_line..=line`; when they are unknown (not every line is in a hunk) the suggestion's balance must be zero for each pair.
  - Posted inline comment: `**{severity label}:** {title}`, then the Finder's `body`, then the optional ```` ```suggestion ```` fence.
- **ReAct Agent Loop (`crates/review/src/agent.rs`):**
  - Enters wrap-up (read-only tools dropped, only the submit tool offered + one reminder, `You are near the end of your budget. Submit your final result now using {submit}.`) on the last `wrap_up_steps`, when the remaining stage deadline is `< 2 × slowest_step`, or when `TokenBudget` is exhausted (after ≥ 1 step). `no_tools_on_last_step` runs (Verifier) instead send, on their final step, a user message asking for the JSON result as text and offer no tools.
  - Every note (coverage debt, budget band, verdict-due) is appended to the conversation and kept: the history is append-only. Notes are sent only with the repository tools and outside wrap-up.
  - Shortens oldest tool results exceeding `COMPACT_EXCERPT_CHARS + COMPACTED_MARKER.chars().count()` (`446` chars, and not already ending with `COMPACTED_MARKER`) to a `400`-char excerpt plus `"\n[older tool output shortened to save context]"` when prompt exceeds `DEFAULT_COMPACTION_CHAR_LIMIT = 320_000` chars (the first prompt and the newest `COMPACT_KEEP_TAIL = 4` messages are never compacted); performs one-shot aggressive compaction on `ContextOverflow`.
  - **Live tracing:** with `RUST_LOG=...,adus_review::trace=trace` the loop traces, secret-redacted, each run's prompt and per step the model's reasoning (`ChatResponse::reasoning()`: OpenAI-compatible `reasoning_content` / `reasoning` / `reasoning_details`, Anthropic thinking blocks), reply text, tool calls and tool results (cut to 2,000 chars). The Finder and Verifier runs sit in `finder` / `verify` debug spans naming specialist, batch, pass or candidate.
- **Token Ceiling (`crates/review/src/budget.rs`):**
  - Shared `TokenBudget` (`settings.max_review_input_tokens`, `0` = off) counts the `input_tokens` of every successful LLM call across Finder (both passes), Prose Recovery, Verifier, Dedup, Severity and Enrichment (soft cap: overshoot is in-flight calls + ≤ 1 wrap-up/agent + diff-only verifications). It is **cache-weighted**: `TokenBudget::add(input, cache_read, cache_write)` charges `uncached + cached.div_ceil(CACHE_READ_DIVISOR = 4)` with `cached = cache_read.min(input)`, so prompt-cache reads cost a quarter; cache writes are charged in full as part of `input`. `input_tokens` is total input with cached tokens included (§6.1), so the limit compares weighted tokens, and `budget.used()` is the weighted figure while the raw input, cache reads and cache writes are summed separately for the stage log.
  - Once exhausted: running agents wrap up, unstarted Finder batches are skipped (`token_budget`, non-retryable), no Pass 2 starts (`SYNTHESIS_MIN_BUDGET_SHARE`), unstarted Verifier candidates run diff-only and no Evidence Gate run starts, while Dedup, Severity and Enrichment still run (one call per finding for Enrichment). Summary adds `"Token budget reached ({used} of {limit} input tokens): {details}."` only when the ceiling actually altered execution (`{used}` is the weighted count compared with `{limit}`; details: batches not reviewed, `some reviewers stopped early`, findings verified from the diff only).
- **Stage log (`review stages finished`, info):** `review_id`, `review_mode`, `clone_ms`, `fallback_reason`, `finder_tasks`, `finder_steps` and `gate_refusals` (summed over the Finder tasks that finished, both passes), `filtered_dropped`, `synthesis_candidates` and `synthesis_skipped`, `verify_candidates` (queued), `verified_kept`, `dropped_refuted`, `dropped_unverified`, `evidence_gate_runs`, `dedup_dropped`, `already_raised`, `posted`, `kept_by_origin` (`pass1=N synthesis_rescue=N prose_recovery=N`), `budget_used` (weighted), `input_tokens` (raw, cached included), `cache_read_tokens`, `cache_write_tokens` (these three are **review-wide**, from the `TokenBudget`, like `budget_used`: they include the Verifier and the post-verify calls, not only the Finder) and `token_budget_reached`.
- **Failure & Retry Classification:**
  - `Retryable` (row deleted): all Finder tasks failed with `AllKeysFailed`/`NoKeys`/`Internal`; or all Verifier tasks failed retryably (`all_retryable_error`); or in `Generalist` mode, some Finder tasks failed and every failure was retryable. `Specialists` mode posts surviving findings with notes.
  - `Fatal`: deadline cut all Finder tasks (`review timed out`), all Finder tasks failed with `token_budget` as first non-retryable failure (`review stopped: token budget reached`), or unrecoverable error.
  - Posting errors: GitHub rate-limit/5xx/network → `Retryable`; GitHub other 4xx / all Azure post errors → `Fatal`.

### 8.5 Learning from Closed PRs (`crates/review/src/learn.rs`)

- Runs for `Job::LearnPr`. Silent: never comments on the PR.
- **Nothing happens (no run row):** repo missing/deleted or disabled, or `learning_mode = off` (so a later mode change can still learn that PR); the PR head already has a `pr_learning_runs` row. PR not found → `Fatal`.
- **Recorded outcome (one row per PR head):** `skipped` when no provider/model resolves (same rule as reviews), `nothing_to_learn` when no thread qualifies or no valid exception remains, `learned` with `learned_count`, `failed` for an unparseable answer or a non-retryable LLM error. Retryable LLM, git and DB errors return `Retryable` with no row.
- **Qualifying threads:** ≥ 2 comments from ≥ 2 authors (Adus's comments count as author `"adus"`), first 60 in provider order, cut to the same 40,000-char budget and formatted as in §8.4.
- **Extraction call:** fast model, `Low` effort, ≤ 8,192 output tokens (OpenAI counts reasoning tokens in it); prompt sections `<threads>`, `<known_memories>` (global + repo memories of every status except `rejected`, enabled or not) and `<rejected_memories>` (memories with status `rejected`), all wrapped as data; each memory list is newest first and stops before the memory that would take its JSON past 20,000 chars; answer `{"exceptions": [{"content", "thread", "defended_by": [comment index]}]}`.
- **Validation:** unknown thread or empty / > 2,000-char content dropped; `defended_by` keeps only existing comments not by Adus, and an exception with none left is dropped; at most 5 per PR head.
- **Storing:** each exception becomes a repo-scoped memory (`source = pr_learning`, `source_url` = thread `url`, `NULL` on Azure). In `auto` mode the memory is `active` only when (a) a `defended_by` author is a registered, non-disabled Adus user (same matching as `@adudu` commands; the Azure author is matched as email) — the first such author becomes `created_by` — and (b) every non-Adus comment author in that thread is also such a user (so an outsider's reason cannot go live by the model citing a maintainer's reply); otherwise, and always in `approval` mode, it is `pending` with no creator. Comments by Adus never count as a defender.
- Memory inserts and the run row are not one transaction (a DB error between them may store exceptions twice on retry).

---

## 9. Dashboard, CLI & Deployment

- **Dashboard pages (`web/`):** Login, Setup, Repositories (connections + repos), AI Providers (providers + keys), Review Rules (Rules & Memory tabs), Reviews, Token Usage, Users (admin), Settings (admin), System Status (admin doctor), Profile. See [DESIGN.md](./DESIGN.md) and [web/AGENTS.md](../web/AGENTS.md).
  - Repositories table: per-repo **Learning** select (`Off` / `Needs approval` / `Automatic`).
  - Review Rules tabs: Rules, Memory (`active` only), **Pending** (learned memories awaiting approval, count badge; visible to all, actions admin-only: Approve, Edit (edit-and-approve dialog), Reject), **Rejected** (admin only; Delete). "View thread" appears only when `source_url` is an http(s) URL.
- **CLI subcommands (`crates/adus`):**
  - `adus serve` (default): sweeps orphaned clone workspaces + stale reviews, runs HTTP server + workers + scheduler.
  - `adus gen-secret`: prints 32 random base64 bytes.
  - `adus check-config`: offline env/URL validation.
  - `adus doctor`: reports `Reviews: OK | DEGRADED | NOT RUNNING` checking config, DB/migrations, Redis, worker heartbeats, default provider/model/keys, Connection PATs, repo webhooks, and `git >= 2.31` (warn if missing/old).
  - `adus healthcheck`: container health probe against `/healthz`.
  - `adus rereview <repository_id> <pr_number>` (**hidden** from `--help`, debug only): connects to Redis and queues `Job::ReviewPr` with `Trigger::Opened` and no mode override, so a running `adus serve` reviews the PR in full (no incremental diff against the last reviewed head) as if it had just been opened. Findings Adus already posted are still filtered by Semantic Dedup (`already_raised`). It is how a live acceptance run is repeated, with `RUST_LOG=info,adus_review=debug,adus_review::trace=trace` (§8.4).
- **Docker & Installers:** Multi-arch GHCR image `ghcr.io/return-nol/adus`. `docker-compose.yml` supports `bundled` (Postgres + Redis) and optional `https` (Caddy) profiles. `scripts/install.sh` and `scripts/install.ps1` support `--build` / `-Build` when prebuilt images cannot be pulled.

---

## 10. Historical Change Specs (Consolidated Archive)

| Date | Spec | Status |
|---|---|---|
| 2026-11-19 | [2026-11-19-adus-design.md](superpowers/specs/2026-11-19-adus-design.md) | Consolidated (v1 core platform) |
| 2026-11-20 | [2026-11-20-ignored-files-and-azure-project-design.md](superpowers/specs/2026-11-20-ignored-files-and-azure-project-design.md) | Consolidated (Azure `project_name`, ignore paths & title keywords) |
| 2026-11-21 | [2026-11-21-agentic-review-engine-design.md](superpowers/specs/2026-11-21-agentic-review-engine-design.md) | Consolidated (ephemeral clone, ReAct Finder/Verifier/Dedup, Small PR, token ceiling) |
| 2026-11-21 | [2026-11-21-provider-models-and-reasoning-design.md](superpowers/specs/2026-11-21-provider-models-and-reasoning-design.md) | Consolidated (`primary_model`/`fast_model`, model discovery, `ReasoningEffort`) |
| 2026-11-23 | [2026-11-23-pr-discussion-awareness-and-learning-design.md](superpowers/specs/2026-11-23-pr-discussion-awareness-and-learning-design.md) | Consolidated (`list_pr_threads`, Dedup against PR discussion, learning from closed PRs, memory approval, `learning_mode`) |
| 2026-11-24 | [2026-11-24-reasoning-effort-wiring-and-replay-design.md](superpowers/specs/2026-11-24-reasoning-effort-wiring-and-replay-design.md) | Consolidated (adaptive Claude thinking, Gemini 3+/4+ thinkingLevel, larger reasoning reserves + cap fallback, OpenAI-compatible reasoning replay); its OpenRouter handling is superseded by the 2026-11-25 spec (§6.1) |
| 2026-11-24 | [2026-11-24-agentic-prompt-and-harness-design.md](superpowers/specs/2026-11-24-agentic-prompt-and-harness-design.md) | Consolidated (anti-CTF role prompts & playbooks, `Rules` activation on `repo_instructions`, 320K/400-char excerpt compaction, `submit_findings` anchor bounce + `filtered_dropped`, Verifier PR-intent/memory/hedge rules & `drop_suggestion`, ±15-line redacted hunk context, since removed by the 2026-11-25 spec); §12 amendment: Finder reports / Verifier filters, `dismissed` log |
| 2026-11-25 | [2026-11-25-review-engine-overhaul-design.md](superpowers/specs/2026-11-25-review-engine-overhaul-design.md) | **Core loop implemented; spec stays `Status: approved`** until the Sharded Rules plan lands. Folded into §§2.1, 3.1, 6.1, 7, 8.2–8.4, 9: OpenRouter `reasoning` + `require_parameters`, Claude `display: "summarized"`, 20 min / 55-80-93 % deadlines, raw hunks + `<changed_functions>`, per-hunk `CoverageLedger` with append-only debt note and one-shot gate, budget bands, required `reasoning`, Synthesis Rescue and Prose Recovery, `primary`/`High` Verifier with Evidence Gate and uncapped `verify_order` queue, post-verify Severity / Enrichment / `check_fix`, cache-weighted `TokenBudget`, hidden `rereview`. **Not yet built (companion plan `2026-11-25-review-engine-sharded-rules`):** Sharded Rules + Evidence Collector/Judge, which will replace `SpecialistKind::Rules` and move repository-instruction checks to `Bug` in `Specialists` mode; until then `Rules` and `<rules>` in the Finder prompts behave as described in §8.4 |
