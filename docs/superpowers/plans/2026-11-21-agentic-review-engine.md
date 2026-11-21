# Agentic Review Engine Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Upgrade Adus's review engine from single-pass, per-file diff review to a two-pass, codebase-aware agentic pipeline (Point 1 + Point 2 + repo instruction files): ephemeral hardened shallow git cloning, read-only pure-Rust codebase tools (`read_file`, `grep`, `list_dir`, `find_files`), multi-turn tool calling across all 4 LLM providers with Anthropic prompt caching, configurable `generalist` vs `specialists` (`Bug`, `Security`, `Performance`, conditional `Rules`) modes, per-finding tool-enabled Verifier, PR-wide Semantic Deduplication, and graceful diff-only fallback.

**Architecture:** `adus-llm` expands from single `(system, user)` strings to multi-turn `Message` / `ToolDef` / `ToolCall` with cache-token accounting and `ToolsUnsupported` / `ContextOverflow` error classification. `adus-git` exposes `CloneSpec` for PAT-authenticated PR fetches. `adus-review` adds `workspace` (RAII hardened shallow clone + `RepoPath` newtype), `tools` (pure-Rust read-only tools + shared `ToolCache`), `repo_instructions` (auto-loaded `AGENTS.md` / `CLAUDE.md` / `.github/copilot-instructions.md` / `.cursor/rules/*.mdc`), `agent` (ReAct loop + context compaction), `specialists`, `verifier`, and `dedup`, wired through `Engine::review_pr` inside `REVIEW_TIMEOUT`. `adus-db`, `adus-server`, `adus`, `Dockerfile`, and `web/` wire `review_mode` config, startup orphan cleanup, `git` binary doctor checks, and UI controls.

**Tech Stack:** Rust 2024 (`tokio` with `process`/`fs`, `reqwest`, `serde`, `serde_json`, `sqlx` runtime queries, `tempfile`, `ignore`, `regex`, `globset`, `thiserror`), Postgres, Redis, SvelteKit 3 + Svelte 5 + shadcn-svelte (`DESIGN.md`), Docker (`debian:bookworm-slim` + `git`).

**Spec:** [2026-11-21-agentic-review-engine-design.md](../specs/2026-11-21-agentic-review-engine-design.md) (also binding: [2026-11-19-adus-design.md](../specs/2026-11-19-adus-design.md), `AGENTS.md`, `DESIGN.md`).

---

## Global Constraints & Rust Standards

- **AGENTS.md baseline:** Runtime `sqlx::query` / `sqlx::query_as` only (never `query!` macros); `ApiJson<T>` / `ApiPath<T>` / `ApiQuery<T>` and `ApiError` JSON errors in `crates/server`; `#lib/*.js` / `#lib/*.svelte` imports and `DESIGN.md` tokens in `web/`; no browser automation; `cargo test --workspace -j 2` on Windows (and clean `target/debug/incremental` if disk space drops).
- **Clean-room policy:** Strictly follow Section 6.13 of the base design spec (`2026-11-19-adus-design.md`) — zero code, prompts, or text derived from Kodus `libs/ee/**` or `*.ee.*`.
- **Rust Best Practices (spec Section 5):**
  - Idiomatic expressive Rust is encouraged over the original minimal learner subset whenever it improves safety or clarity.
  - **Borrowing & ownership:** Take `&str`, `&[T]`, `Cow<'_, str>` at boundaries; pass `Copy` enums (`ReviewMode`, `Severity`, `SpecialistKind`) by value; avoid `.clone()` inside hot search/loop iterations.
  - **Type-driven invariants:** `RepoPath` only via `RepoWorkspace::resolve`; RAII `RepoWorkspace` owning `tempfile::TempDir`; typed serde structs (`ReadFileArgs`, `GrepArgs`, etc.) for tool inputs/outputs instead of untyped `serde_json::Value` indexing.
  - **Error handling:** Typed `thiserror` error enums per boundary (`WorkspaceError`, `ToolError`, `AgentError`, `LlmError`, `ChatError`); zero `.unwrap()`/`.expect()` outside tests; `?` propagation.
  - **Async discipline:** Run `ignore::WalkBuilder`, file reads, and regex scans inside `tokio::task::spawn_blocking`; use `std::sync::RwLock` (never held across `.await`) for `ToolCache`.
  - **Dispatch:** Static dispatch / closed enums internally; reserve `dyn Trait` for true runtime/test seams (`GitProvider`, `GitProviderFactory`, `LlmClient`, `ChatService`, `RepoCloner`, `JobRunner`).
  - **Linting:** `cargo clippy --workspace --all-targets -- -D warnings` must pass cleanly (`redundant_clone`, `large_enum_variant`, `needless_collect`; use `#[expect(clippy::..., reason = "...")]` if an exception is ever justified).
- **Crate acyclicity:** Keep `core` < (`db`, `git`, `queue`), (`core`, `db`) < `llm`, (`core`, `db`, `git`, `llm`, `queue`) < `review`, and (`server`, `adus`) on top.
- **Model choice for subagents:** Tasks 2, 3, 4, 5, 6, 7, 8 touch `crates/llm`, clone security, ReAct loop, prompts, verifier, and `Engine` → **opus, effort high**. Tasks 1, 9, 10 → **sonnet, effort high**.

---

## Review Focus (Critical Invariants)

1. **Path traversal, symlink escape, `.git/`, and secret-file refusal:** Tools and `RepoWorkspace::resolve` must reject `..`, leading `/`, Windows drives, symlinks escaping `canonical_root`, `.git/**`, and secret files (`.env`, `.env.*` except `.env.example`/`.sample`/`.template`, `*.pem`, `*.key`, `*.p12`, `*.pfx`, `id_rsa*`, `id_ed25519*`, `*.kdbx`, `.npmrc`, `.netrc`, `secrets.yml`/`secrets.yaml`). `GitCliCloner` must pass `-c core.hooksPath=/dev/null -c core.symlinks=false -c core.fsmonitor=false -c filter.lfs.smudge= -c filter.lfs.process= -c protocol.file.allow=never` and `--no-recurse-submodules`. Verify in Tasks 3 & 4.
2. **PAT hygiene:** PAT must pass only through the fetch command's environment (`GIT_CONFIG_COUNT=1`, `GIT_CONFIG_KEY_0=http.extraHeader`, `GIT_CONFIG_VALUE_0=Authorization: ...`), never in argv, `.git/config` or the remote URL, and be scrubbed from any error string or tracing output. `git` older than 2.31 → `WorkspaceError::Unavailable` (diff-only fallback, never `Unauthorized`). Verify in Task 3.
3. **Queue JSON backwards compatibility:** In-flight `Job::ReviewPr` messages without `"mode_override"` must deserialize cleanly with `mode_override: None`. Verify in Task 1.
4. **Graceful fallback vs. auth failure:** Clone timeout/error (non-auth) or `ChatError::ToolsUnsupported` falls back to diff-only self-contained mode (with diff-only Verifier + Semantic Dedup) and adds a summary note (`Codebase context unavailable — reviewed diff only`). `WorkspaceError::Unauthorized` or `GitError::Unauthorized` still marks the `Connection` `invalid` and returns `EngineError::Fatal`. Verify in Task 8.
5. **Fail-closed Verifier & safe Semantic Dedup:** A failed or unparseable Verifier call **drops** the unverified candidate and adds a summary warning (never publishes unverified high-recall output); if all candidates fail verification due to retryable LLM errors (`AllKeysFailed`/`NoKeys`/`Internal`) and zero succeed, return `EngineError::Retryable`. Semantic Dedup failure or out-of-bounds/empty output falls back safely to the verified list unchanged. Verify in Tasks 7 & 8.
6. **Partial specialist failure:** In `specialists` mode, if ≥ 1 specialist succeeds while another fails, publish verified findings from the surviving specialists with a summary warning; if all fail retryably, propagate `EngineError::Retryable`. Verify in Task 8.
7. **Provider wire specifics:** Anthropic prompt caching sets `cache_control: {"type": "ephemeral"}` on system block, last tool, and latest user/tool checkpoint, and adds `cache_read_input_tokens + cache_creation_input_tokens` into `ChatResponse.input_tokens` (while exposing breakdown fields). Gemini preserves `thoughtSignature` (`provider_meta`) on model tool calls and synthesizes call IDs when absent. Verify in Task 2.
8. **Timeout & RAII bounds:** Preserve compile-time invariants `REVIEW_TIMEOUT (12m) < JOB_TIMEOUT (13m) < LOCK_TTL (15m)` and `JOB_TIMEOUT < reviews::STALE_AFTER (15m)`. Dropping/aborting a timed-out job guarantees `RepoWorkspace` deletes its temp dir, and `cleanup_orphaned_workspaces()` sweeps crash leftovers at startup. Verify in Tasks 3 & 8.
9. **Stage deadlines (spec 4.11):** Finder (incl. clone) by 60 % of `review_timeout`, Verifier by 90 %, Dedup by 97 %. A cut Finder task counts as a failed specialist (`"{label} reviewer did not finish in time"`, `Fatal("review timed out")` only when no Finder task finished); a cut Verifier candidate is dropped as unverified (never retryable); a cut Dedup keeps the verified list. Verify in Tasks 7 & 8.

---

## Task Overview

| # | Scope | Primary Crates / Areas |
|---|---|---|
| 1 | `ReviewMode`, DB migration `0008`, `Job::ReviewPr.mode_override`, `@adudu review deep` | `core`, `db`, `migrations`, `review/command`, `server/webhooks`, `adus/worker` |
| 2 | Multi-turn `Message`/`ToolDef`/`ToolCall`, error classification, Anthropic prompt caching | `llm`, `server/api/ai` |
| 3 | `CloneSpec` on `GitProvider`, hardened `GitCliCloner`, `RepoWorkspace` RAII, `RepoPath` newtype | `git`, `review/workspace` |
| 4 | Pure-Rust read-only tools, `ToolCache`, secret/binary guards, `repo_instructions` loader | `Cargo.toml`, `review/tools`, `review/repo_instructions` |
| 5 | Reusable ReAct agent loop, concurrent tool execution, context compaction & overflow recovery | `review/agent` |
| 6 | Prompt architecture, PR manifest, 40K diff batching, `specialists` prompts, `keep_valid_batch`, summary warnings | `review/prompt`, `review/parse`, `review/specialists` |
| 7 | Per-finding tool-enabled Verifier (fail-closed) & PR-wide Semantic Dedup (safe fallback) | `review/verifier`, `review/dedup` |
| 8 | Full `Engine` pipeline wiring, `ToolsUnsupported`/clone fallback, startup sweep, `doctor`, `Dockerfile` | `review/engine`, `adus/main`, `server/doctor`, `Dockerfile` |
| 9 | Settings & Repository `review_mode` REST APIs + server integration tests | `server/api/settings`, `server/api/repositories` |
| 10 | Dashboard UI for `review_mode` (`Settings` + `Repositories`) & full workspace verification | `web/routes/settings`, `web/routes/repositories` |

---

### Task 1: `ReviewMode`, Migration `0008`, Job Override & `@adudu review deep`

**Files:**
- Create: `migrations/0008_review_mode.sql`
- Modify: `crates/core/src/types.rs`, `crates/core/src/job.rs`, `crates/db/src/settings.rs`, `crates/db/src/repositories.rs`, `crates/review/src/command.rs`, `crates/server/src/webhooks.rs`, `crates/adus/src/worker.rs`
- Test: `crates/core/src/types.rs`, `crates/core/src/job.rs`, `crates/db/tests/` (settings/repositories DB tests), `crates/review/tests/command.rs`

**Interfaces:**
- Produces:
  - `adus_core::types::ReviewMode { Generalist, Specialists }` (`Copy`, `Default = Generalist`, `sqlx::Type` `type_name = "review_mode"`, `snake_case` serde).
  - `Job::ReviewPr { repository_id: Uuid, pr_number: i64, trigger: Trigger, #[serde(default)] mode_override: Option<ReviewMode> }`.
  - `Settings.review_mode: ReviewMode` (in `crates/db/src/settings.rs`; `update()` binds `$7`).
  - `Repository.review_mode: Option<ReviewMode>` and `RepoPatch.review_mode: Option<Option<ReviewMode>>` (in `crates/db/src/repositories.rs`, same `CASE WHEN $8 THEN $9 ELSE review_mode END` pattern as `review_provider_id` / `review_model`).
  - `Command::Review { mode: Option<ReviewMode> }` in `crates/review/src/command.rs`.
  - `Engine::review_pr(&self, repository_id: Uuid, pr_number: i64, trigger: Trigger, mode_override: Option<ReviewMode>) -> Result<Option<Uuid>, EngineError>` (pass `None` in `webhooks.rs` and existing callers until Task 8 expands `review_pr`; update `worker.rs` to forward `mode_override`).

**Edge Cases & Pitfalls:**
- **Queue backwards compatibility:** Legacy Redis jobs lack `mode_override`; `#[serde(default)]` must deserialize them as `None` (and let existing `webhooks.rs` tests pass unchanged when constructing `Job::ReviewPr` with helper constructors or updating struct literals).
- **Command word boundaries:** `@adudu review deep`, `@adudu review specialists`, `@adudu REVIEW DEEP!` → `Some(ReviewMode::Specialists)`. `@adudu review generalist` → `Some(ReviewMode::Generalist)`. `@adudu review please` or `@adudu review` → `None` (respects configured mode). `@adudu review deeper` must NOT match `deep` (reuse whole-word boundary check like `word()`).
- **Update `HELP` text** in `crates/review/src/command.rs` to include `- \`@adudu review deep\` — run a multi-specialist deep review (Bug, Security, Performance, Rules)`.

- [ ] **Step 1: Write failing tests**
  - `crates/core/src/types.rs`: `review_mode_serializes_snake_case` (`"generalist"`, `"specialists"`).
  - `crates/core/src/job.rs`: `job_review_pr_without_mode_override_deserializes_none` (Review Focus #3) + round-trip with `Some(ReviewMode::Specialists)`.
  - `crates/review/tests/command.rs`: `parse_review_deep_and_specialists` (`@adudu review deep`, `@adudu review specialists`, `@adudu review generalist`, `@adudu review please` → `None`, `@adudu review deeper` → `None`) + verify `Command::Review { mode: Some(ReviewMode::Specialists) }` enqueues `Job::ReviewPr` with `mode_override: Some(ReviewMode::Specialists)`.
  - `crates/db` test: fresh `settings` row has `review_mode == ReviewMode::Generalist`; `repositories::update` can set `Some(Some(ReviewMode::Specialists))` and clear `Some(None)`.
- [ ] **Step 2:** Run `cargo test -j 2 -p adus-core -p adus-db -p adus-review` → FAIL (compile/assertion failure).
- [ ] **Step 3:** Implement `migrations/0008_review_mode.sql`, `ReviewMode`, `Job::ReviewPr`, `settings.rs`, `repositories.rs`, `command.rs`, `webhooks.rs`, `worker.rs`, and update `Engine::review_pr` signature (forwarding/ignoring `mode_override` cleanly until Task 8) plus test struct literals (`crates/review/tests/engine.rs`, `crates/server/src/webhooks.rs`, `crates/adus/tests/worker.rs`).
- [ ] **Step 4:** Run `cargo test -j 2 -p adus-core -p adus-db -p adus-review -p adus-server -p adus` → PASS.
- [ ] **Step 5:** Commit `feat: add review_mode config, DB columns, and @adudu review deep command`

---

### Task 2: Multi-Turn Tool Calling & Anthropic Prompt Caching (`crates/llm`)

**Files:**
- Modify: `crates/llm/src/types.rs`, `crates/llm/src/lib.rs`, `crates/llm/src/openai.rs`, `crates/llm/src/anthropic.rs`, `crates/llm/src/gemini.rs`, `crates/llm/src/router.rs`, `crates/server/src/api/ai.rs`
- Test: `crates/llm/tests/clients.rs` (new wiremock integration tests), `crates/llm/tests/router.rs`, `crates/review/tests/common/mod.rs`

**Interfaces:**
- Produces in `adus_llm::types` (re-exported in `adus_llm::lib`):
  - `Message::{User { parts: Vec<UserPart> }, Assistant { parts: Vec<AssistantPart> }, Tool { results: Vec<ToolResult> }}`
  - `UserPart::Text(String)`
  - `AssistantPart::{Text(String), ToolCall(ToolCall)}`
  - `ToolCall { id: String, name: String, arguments: serde_json::Value, provider_meta: Option<String> }`
  - `ToolResult { call_id: String, tool_name: String, content: String, is_error: bool }`
  - `ToolDef { name: String, description: String, parameters: serde_json::Value }`
  - `StopReason::{EndTurn, ToolUse, MaxTokens, Other}`
  - `ChatRequest { model: String, system: String, messages: Vec<Message>, tools: Vec<ToolDef>, max_tokens: u32 }` + `ChatRequest::text(model, system, user, max_tokens) -> Self`
  - `ChatResponse { text: String, tool_calls: Vec<ToolCall>, stop_reason: StopReason, input_tokens: u32, output_tokens: u32, cache_read_tokens: u32, cache_write_tokens: u32 }`
  - `LlmError` adds `ToolsUnsupported(String)` and `ContextOverflow(String)`.
  - `ChatError` adds `ToolsUnsupported(String)` and `ContextOverflow(String)`.
- Consumes in `crates/server/src/api/ai.rs`: update `test_key` to use `ChatRequest::text(...)` and add `error_kind` mappings `"tools_unsupported"` and `"context_overflow"`.

**Edge Cases & Pitfalls:**
- **Empty response check:** `non_empty` must succeed whenever `!tool_calls.is_empty()` even if `text` is empty! Only return `LlmError::Invalid("empty response")` when both `text.trim().is_empty()` and `tool_calls.is_empty()`.
- **ToolsUnsupported detection:** Inspect 400/404/422 error messages case-insensitively **only when `!req.tools.is_empty()`** for markers like `"does not support tools"`, `"tool_use is not supported"`, `"tools is not supported"`, `"function calling is not supported"`, `"tool calling is not supported"`, `"unknown field"` + `"tools"`.
- **ContextOverflow detection:** Inspect 400/413/422 error messages case-insensitively for `"context_length_exceeded"`, `"maximum context length"`, `"prompt is too long"`, `"exceeds the maximum"`, `"too many tokens"`, `"token count exceeds"`, `"input is too long"`. Also map HTTP 413 (`PAYLOAD_TOO_LARGE`) in `send()` when JSON body indicates context overflow (or map 413 cleanly).
- **Anthropic prompt caching & token accounting (Review Focus #7):**
  - Send `"system": [{"type": "text", "text": req.system, "cache_control": {"type": "ephemeral"}}]` when `!req.system.is_empty()`.
  - Attach `"cache_control": {"type": "ephemeral"}` on the **last** element of `"tools"` when `!req.tools.is_empty()`.
  - When `messages.len() >= 2`, attach `"cache_control": {"type": "ephemeral"}` on the **last content block** of the last `User`/`Tool` message (stays ≤ 3 breakpoints, under Anthropic's limit of 4).
  - Map `usage.input_tokens + cache_read_input_tokens + cache_creation_input_tokens` into `ChatResponse.input_tokens`, and populate `cache_read_tokens` / `cache_write_tokens`.
- **Gemini `thoughtSignature` & synthetic call IDs (Review Focus #7):**
  - Gemini `Part` deserializes `functionCall: Option<GeminiFnCall>` and `thoughtSignature: Option<String>`. When a part has `functionCall`, assign the synthetic ID `format!("gc_{prior}_{idx}")`, where `prior` is the number of `ToolCall`s already in `req.messages` and `idx` is the call's index within this response (unique across the conversation), and store `thoughtSignature` in `ToolCall.provider_meta`.
  - When serializing `Message::Assistant` back to Gemini `"role": "model"`, include `"thoughtSignature": meta` on the corresponding `functionCall` part if `provider_meta.is_some()`.
  - Do NOT serialize `toolConfig` when `req.tools.is_empty()`.
- **OpenAI malformed tool arguments:** If `function.arguments` is invalid JSON, do not fail the whole response decode; wrap as `json!({"__raw_parse_error": err.to_string(), "raw": raw_args})` so tool dispatch returns `is_error: true` and lets the model retry on the next turn.
- **LlmRouter behavior:** `LlmError::ToolsUnsupported` and `LlmError::ContextOverflow` behave like `Invalid` wrt keys (do NOT mark keys invalid or trigger cooldown), log usage with `"tools_unsupported"` / `"context_overflow"`, and return `ChatError::ToolsUnsupported` / `ChatError::ContextOverflow` immediately.

- [ ] **Step 1: Write failing tests**
  - `crates/llm/tests/clients.rs` (wiremock):
    - OpenAI: sends `tools`, parses `tool_calls`, serializes `role: "tool"` on turn 2, extracts `prompt_tokens_details.cached_tokens`.
    - OpenAI-compatible: uses `max_tokens` (not `max_completion_tokens`), maps `"model does not support tools"` 400 → `LlmError::ToolsUnsupported`, maps `"maximum context length"` 400 → `LlmError::ContextOverflow`.
    - Anthropic: sends `cache_control: {"type": "ephemeral"}` on system block + last tool + checkpoint message, parses `tool_use`, serializes `tool_result` inside `role: "user"`, computes `input_tokens = input + cache_read + cache_creation`.
    - Gemini: sends `functionDeclarations`, assigns unique synthetic IDs across turns, preserves `thoughtSignature` on `functionCall` round-trip, extracts `cachedContentTokenCount`.
  - `crates/llm/tests/router.rs`: `router_does_not_penalize_key_on_tools_unsupported_or_overflow` (key remains `ok`, usage row written with `"tools_unsupported"` / `"context_overflow"`).
- [ ] **Step 2:** Run `cargo test -j 2 -p adus-llm` → FAIL.
- [ ] **Step 3:** Implement `types.rs`, `lib.rs`, `openai.rs`, `anthropic.rs`, `gemini.rs`, `router.rs`, and update `crates/server/src/api/ai.rs` + test helpers (`crates/review/tests/common/mod.rs`).
- [ ] **Step 4:** Run `cargo test -j 2 -p adus-llm -p adus-server` → PASS.
- [ ] **Step 5:** Commit `feat(llm): add multi-turn tool calling, overflow/tools errors, and prompt caching`

---

### Task 3: Hardened Ephemeral Git Cloner & `RepoPath` (`crates/git` + `crates/review`)

**Files:**
- Modify: `Cargo.toml` (add `tempfile = "3"`, `tokio` features `["process", "fs"]`), `crates/git/Cargo.toml`, `crates/git/src/types.rs`, `crates/git/src/lib.rs`, `crates/git/src/github.rs`, `crates/git/src/azure.rs`, `crates/review/Cargo.toml`, `crates/review/src/lib.rs`
- Create: `crates/review/src/workspace.rs`
- Test: `crates/git/tests/github.rs`, `crates/git/tests/azure.rs`, `crates/review/src/workspace.rs` (`mod tests` using local bare git repos in `TempDir`)

**Interfaces:**
- In `adus_git`:
  - `PullRequest` gains `pub source_ref: Option<String>` (Azure populates from `sourceRefName`; GitHub sets `None`). Keep fields public, add no constructor, and update every existing struct literal (`crates/git/tests/*`, `crates/review/tests/common/mod.rs`, `crates/review/src/prompt.rs`, and any others the compiler finds).
  - `CloneSpec { pub url: String, pub refspec: String, pub fallback_refspec: Option<String>, pub expected_sha: String, pub auth_header: String }`.
  - `GitProvider::clone_spec(&self, _repo: &RepoRef, _pr: &PullRequest) -> Option<CloneSpec> { None }` (default `None` so existing trait impls work out of the box; `GithubProvider` and `AzureProvider` implement it).
- In `adus_review::workspace`:
  - `WorkspaceError`: `#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]` (`Unauthorized`, `TooLarge`, `TimedOut`, `Unavailable(String)`).
  - `PathError`: `#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]` (`Invalid(String)`, `EscapesRoot`, `ForbiddenGitPath`, `SecretFile`).
  - `RepoWorkspace` owning `TempDir` + `canonical_root: PathBuf`:
    - `pub fn root(&self) -> &Path`
    - `pub fn resolve(&self, rel: &str) -> Result<RepoPath, PathError>`
    - `pub fn resolve_existing_file(&self, rel: &str) -> Result<RepoPath, PathError>` (checks `is_file()`)
    - `pub fn resolve_dir(&self, rel: &str) -> Result<RepoPath, PathError>` (allows `""` / `"."` → root, checks `is_dir()`)
    - `pub fn from_dir_for_test(dir: TempDir) -> std::io::Result<Self>` (available cleanly for integration tests, or gated helper module; note `#[doc(hidden)]` or public `from_temp_dir` is fine for `crates/review/tests/engine.rs`!)
  - `RepoPath` newtype (`abs: PathBuf`, `rel: String`) with `abs(&self) -> &Path` and `rel(&self) -> &str` (no public constructor; only `RepoWorkspace` creates it!).
  - `#[async_trait] pub trait RepoCloner: Send + Sync { async fn clone_pr(&self, spec: &CloneSpec) -> Result<RepoWorkspace, WorkspaceError>; }`
  - `GitCliCloner` (`clone_timeout: Duration = 60s`, `max_bytes: u64 = 500 MiB`, optional `allow_file_protocol_for_test: bool = false`).
  - `pub async fn cleanup_orphaned_workspaces()` — sweeps `<temp_dir>/adus-clones/*` on startup (non-fatal warnings on IO error).

**Edge Cases & Pitfalls:**
- **GitHub & Azure git URLs:**
  - `GithubProvider`: if `self.base_url == "https://api.github.com"` → `https://github.com/{owner}/{name}.git`; if it ends with `"/api/v3"` (GHES) → strip `"/api/v3"`; otherwise `{base_url}/{owner}/{name}.git`. Build every URL (both providers) with `reqwest::Url` + `path_segments_mut()` so each segment is percent-encoded, and return `None` if any segment is empty. `auth_header`: `Basic {base64("x-access-token:" + self.token)}`. `refspec`: `format!("refs/pull/{}/head", pr.number)`.
  - `AzureProvider`: `{base_url}/{organization}/{project}/_git/{name}`. `auth_header`: `Basic {base64(":" + self.token)}`. `refspec`: `pr.head_sha.clone()`, `fallback_refspec`: `pr.source_ref.clone()`. (Try `spec.refspec`; if `git fetch` fails and `fallback_refspec` is `Some`, try `fallback_refspec` once!)
- **Hardened git flags & environment (Review Focus #1 & #2):**
  - Every `git` command uses `.env_clear()`, passing only safe platform minimal env (`PATH`, Windows `SystemRoot`/`SYSTEM32`/`WINDIR`/`TEMP`/`TMP`, `GIT_TERMINAL_PROMPT=0`, `GIT_CONFIG_NOSYSTEM=1`, `GIT_CONFIG_GLOBAL` pointing to an empty file or `/dev/null`/`NUL`), plus `.kill_on_drop(true)`.
  - Check `git --version` once per process (cache it, e.g. `tokio::sync::OnceCell`); below 2.31 → `WorkspaceError::Unavailable("git 2.31 or newer is required")`.
  - `git fetch` passes `-c core.hooksPath=/dev/null -c core.symlinks=false -c core.fsmonitor=false -c filter.lfs.smudge= -c filter.lfs.process=` and `-c protocol.file.allow=never` (unless test-only flag is enabled). The PAT goes only in the fetch command's env: `GIT_CONFIG_COUNT=1`, `GIT_CONFIG_KEY_0=http.extraHeader`, `GIT_CONFIG_VALUE_0=Authorization: {auth_header}` — never in argv. Never pass `-c` with user-controlled strings, and put `--` before `<url>`. Never store remote URL in `.git/config` (`git fetch` directly with URL rather than `git remote add`).
  - Scrub `spec.auth_header` and raw decoded PAT (from Basic header) from any stderr text in `WorkspaceError::Unavailable`.
- **Auth detection:** Map stderr containing `"authentication failed"`, `"401"`, `"403"`, `"could not read username"`, `"terminal prompts disabled"` (when auth required), or `"invalid username or password"` to `WorkspaceError::Unauthorized`.
- **Secret-file check (`is_secret_path`):** Check every path component and filename case-insensitively: `.git`, `.env`, `.env.*` (allow `.env.example`, `.env.sample`, `.env.template`), `*.pem`, `*.key`, `*.p12`, `*.pfx`, `*.kdbx`, `id_rsa*`, `id_ed25519*`, `.npmrc`, `.netrc`, `secrets.yml`, `secrets.yaml`.
- **Symlink escape test:** On Unix (or Windows if symlink creation is permitted, or via `#[cfg(unix)]` + unit test for path normalization), verify symlinks created inside a bare repo are NOT created as symlinks on checkout (`core.symlinks=false`) AND if a symlink exists on disk pointing outside `canonical_root`, `RepoWorkspace::resolve` returns `Err(PathError::EscapesRoot)`.
- **RAII drop verification:** Confirm dropping `RepoWorkspace` deletes its directory immediately from disk.

- [ ] **Step 1: Write failing tests**
  - `crates/git`: `github_clone_spec_uses_pr_ref_and_x_access_token`, `azure_clone_spec_uses_project_guid_and_basic_auth`.
  - `crates/review/src/workspace.rs`:
    - `clones_local_repo_with_hardened_flags_and_deletes_on_drop` (verifies `core.symlinks=false`, hooks in bare repo don't fire, `.git/config` contains no token/remote URL, checkout matches commit, dir deleted on drop).
    - `fetch_puts_auth_header_in_env_not_argv` (unit-test the `Command` builder: the header appears in `GIT_CONFIG_VALUE_0` and in no argument).
    - `falls_back_to_fallback_refspec_when_primary_fetch_fails`.
    - `resolve_rejects_traversal_git_and_secret_files` (`../etc/passwd`, `/etc/passwd`, `.git/HEAD`, `.env`, `.env.production`, `certs/server.pem`, `keys/id_rsa`, while allowing `.env.example` and `src/main.rs`).
    - `resolve_rejects_symlink_escaping_root` (`#[cfg(unix)]`).
    - `scrubs_auth_header_from_git_errors` & maps auth failure to `WorkspaceError::Unauthorized`.
    - `timeout_aborts_clone_and_cleans_up_tempdir`.
    - `cleanup_orphaned_workspaces_removes_leftover_dirs`.
- [ ] **Step 2:** Run `cargo test -j 2 -p adus-git -p adus-review` → FAIL.
- [ ] **Step 3:** Implement `Cargo.toml` dependencies (`tempfile`, `tokio` features), `crates/git` `CloneSpec` + `PullRequest.source_ref`, and `crates/review/src/workspace.rs`.
- [ ] **Step 4:** Run `cargo test -j 2 -p adus-git -p adus-review` → PASS.
- [ ] **Step 5:** Commit `feat(git,review): add CloneSpec and hardened ephemeral RepoWorkspace`

---

### Task 4: Pure-Rust Read-Only Tools, `ToolCache` & Repo Instruction Loader

**Files:**
- Modify: `Cargo.toml` (add `ignore = "0.4"`, `regex = "1"`), `crates/review/Cargo.toml`, `crates/review/src/lib.rs`
- Create: `crates/review/src/tools.rs`, `crates/review/src/repo_instructions.rs`
- Test: `crates/review/src/tools.rs` (`mod tests`), `crates/review/src/repo_instructions.rs` (`mod tests`)

**Interfaces:**
- In `crates/review/src/tools.rs`:
  - `Toolbox`: holds `workspace: Arc<RepoWorkspace>`, `cache: Arc<ToolCache>`, `read_only_defs: Vec<ToolDef>` (cached once).
  - `ToolCache`: thread-safe `std::sync::RwLock<HashMap<CacheKey, String>>` shared across all specialists and verifiers of a single review.
  - Typed input structs (`#[derive(Deserialize)]`):
    - `ReadFileArgs { path: String, start_line: Option<u32>, end_line: Option<u32> }`
    - `GrepArgs { pattern: String, path: Option<String>, glob: Option<String>, case_sensitive: Option<bool> }`
    - `ListDirArgs { path: Option<String> }`
    - `FindFilesArgs { pattern: String, path: Option<String> }`
  - `ToolOutput { pub content: String, pub is_error: bool }`
  - `impl Toolbox`:
    - `pub fn read_only_tool_defs(&self) -> &[ToolDef]`
    - `pub async fn execute_read_only(&self, name: &str, args: &serde_json::Value) -> ToolOutput`
    - `pub fn read_file_snippet_sync(&self, path: &str, center_line: u32, radius: u32) -> Option<String>` (used by Verifier to build initial evidence bundle!).
- In `crates/review/src/repo_instructions.rs`:
  - `pub async fn load_repo_instructions(workspace: Arc<RepoWorkspace>) -> Option<String>` (runs in `spawn_blocking`, reads `AGENTS.md`, `CLAUDE.md`, `.github/copilot-instructions.md`, and lexicographically sorted `.cursor/rules/*.mdc` via `workspace.resolve_existing_file`, skips missing/binary, formats each with `--- <path> ---`, caps total at `8_000` chars).

**Edge Cases & Pitfalls:**
- **Blocking work off Tokio reactor:** All `Toolbox::execute_read_only` methods clone `Arc<RepoWorkspace>` + `Arc<ToolCache>` and run inside `tokio::task::spawn_blocking`. Never hold `RwLock` across `.await`.
- **`read_file` caps & guards:** Reject files > 2 MB with actionable message (`"file exceeds 2 MB limit; use grep instead"`). Check first 8 KB for `0x00` byte → reject binary files. Normalize CRLF/LF cleanly, cap output at `200` lines and `8_000` chars, and include header `[<path>: lines <start>-<end> of <total>]` so the model knows whether more lines follow.
- **`grep` safety:** Use `regex::RegexBuilder::new(&pattern).size_limit(10 * 1024 * 1024)` to prevent exponential/huge regex DoS. Use `ignore::WalkBuilder` with `.hidden(false)`? Careful: skip `.git/` explicitly via `filter_entry`, respect `.gitignore` (`git_ignore(true)`), `follow_links(false)`, and skip `is_secret_path` files! Collect up to `40` match groups with `2` context lines around matches (marking match lines with `>` and context with `-` or `:`), capped at `8_000` chars. Skip binary files.
- **`list_dir` & `find_files`:** Skip `.git` and secret paths, sort deterministically, cap at `150` entries / `4_000` chars.
- **Tool schema compatibility:** Keep JSON Schemas clean (`type: "object"`, explicit `properties`, `required` arrays, no `$schema` or complex `oneOf` that trips Gemini/Anthropic/OpenAI-compatible).
- **Cache key normalization:** Normalize arguments before building `CacheKey` (e.g., trim path, default options) so repeated identical reads/greps hit `ToolCache` across specialists and verifiers.

- [ ] **Step 1: Write failing tests**
  - `read_file`: line range windowing, 200-line/8K-char truncation header, binary file refusal, secret `.env` refusal, missing file returns `is_error: true` without panic.
  - `grep`: context lines, path & glob filter, `.gitignore` & `.git/` & `.env` exclusion, 40-group cap, invalid/oversized regex returns `is_error: true`.
  - `list_dir` & `find_files`: hides `.git` and secret files, sorts alphabetically, caps at 150 entries.
  - `ToolCache`: second identical call returns cached result even after checking cache hit counter / inspecting modified temp file.
  - `repo_instructions`: loads `AGENTS.md`, `CLAUDE.md`, `.github/copilot-instructions.md`, sorted `.cursor/rules/*.mdc`, caps at 8,000 chars, ignores missing files.
- [ ] **Step 2:** Run `cargo test -j 2 -p adus-review` → FAIL.
- [ ] **Step 3:** Implement `crates/review/src/tools.rs` and `crates/review/src/repo_instructions.rs`, wire `Cargo.toml` dependencies.
- [ ] **Step 4:** Run `cargo test -j 2 -p adus-review` → PASS.
- [ ] **Step 5:** Commit `feat(review): add pure-Rust read-only repo tools, cache, and instruction loader`

---

### Task 5: Reusable ReAct Agent Loop & Context Compaction (`crates/review/src/agent.rs`)

**Files:**
- Create: `crates/review/src/agent.rs`
- Modify: `crates/review/src/lib.rs`
- Test: `crates/review/src/agent.rs` (`mod tests` using a local `ChatService` fake)

**Interfaces:**
- `AgentConfig`:
  - `pub max_steps: usize`
  - `pub compaction_char_limit: usize` (default `160_000`)
  - `pub max_output_tokens: u32` (default `8_192`)
  - `pub max_concurrent_tools: usize` (default `4`)
- `AgentOutcome<T>`:
  - `pub result: T`
  - `pub steps_used: usize`
  - `pub cache_read_tokens: u32`
  - `pub cache_write_tokens: u32`
  - `pub cut_short: bool`
- `AgentError`:
  - `ToolsUnsupported(String)`
  - `Chat(ChatError)`
  - `Unparseable(String)`
- `TerminalTool<T>` (closure or small trait/struct pairing `ToolDef` + `Fn(&serde_json::Value) -> Result<T, String>` + prose fallback parser `Fn(&str) -> Result<T, String>`):
  - `pub async fn run_react_loop<T, FParse, FFallback>(...) -> Result<AgentOutcome<T>, AgentError>`
  - Or generic over `S: SubmitSpec<Output = T>` (clean static dispatch per `/rust-best-practices`!).

**Edge Cases & Pitfalls:**
- **Concurrent tool execution:** When the assistant emits multiple read-only tool calls in one turn (e.g., 3 `read_file` calls), run them with `futures::stream::iter(...).buffered(max_concurrent_tools)` so order in `Message::Tool { results }` matches `tool_calls` order (required by Anthropic/OpenAI).
- **Terminal tool + read-only tools in same turn:** If the assistant calls `submit_findings` AND a read-only tool in the same turn, check the submit tool first; if valid, return immediately without executing extra reads (or if invalid, execute reads + return `is_error: true` for the malformed submit).
- **Step budget wrap-up turn:** On `step == max_steps - 1`, remove all read-only tools from `ChatRequest.tools`, leaving **only** the terminal submit tool, and append a short user reminder (`"You are at the final step. Submit your final result now using <submit_tool_name>."`).
- **Prose fallback when model skips submit tool:** If the model stops with `tool_calls.is_empty()`, run `fallback_parse(&response.text)` first; if it fails and `step + 1 < max_steps`, send one nudge (`"Please submit your result by calling <submit_tool_name> (or return the JSON directly)."`) before declaring `Unparseable`.
- **Context compaction:** Before each `chat()` call, while `messages_char_len(&messages) > config.compaction_char_limit`: walk `messages[1..messages.len().saturating_sub(4)]` replacing the oldest `ToolResult.content` > 120 chars with `"[earlier tool output omitted to save context]"`. If nothing left to compact, break cleanly (avoid infinite loop!).
- **Context overflow recovery:** If `chat()` returns `Err(ChatError::ContextOverflow(_))` and `!overflow_retried`: aggressively compact (keep `messages[0]`, plus the last 2 messages with tool outputs truncated to 500 chars) and retry once. Second `ContextOverflow` returns `Err(AgentError::Chat(ChatError::ContextOverflow(...)))`.
- **`ToolsUnsupported` fast propagation:** `ChatError::ToolsUnsupported` immediately returns `Err(AgentError::ToolsUnsupported(_))` so `Engine` can trigger diff-only fallback without wasting turns.

- [ ] **Step 1: Write failing tests**
  - `completes_multi_turn_tool_calls_then_submits` (reads file → receives result → calls submit tool).
  - `retries_when_submit_arguments_malformed` (receives `is_error: true`, submits valid JSON next turn).
  - `wraps_up_on_final_step_with_submit_tool_only` (verifies read-only tools are stripped on `max_steps - 1`).
  - `falls_back_to_prose_json_when_model_skips_submit_tool`.
  - `compacts_oldest_tool_outputs_when_exceeding_char_limit` (preserves `messages[0]` and latest 2 turns).
  - `recovers_once_from_context_overflow_error`.
  - `propagates_tools_unsupported_immediately`.
- [ ] **Step 2:** Run `cargo test -j 2 -p adus-review` → FAIL.
- [ ] **Step 3:** Implement `crates/review/src/agent.rs`.
- [ ] **Step 4:** Run `cargo test -j 2 -p adus-review` → PASS.
- [ ] **Step 5:** Commit `feat(review): add generic ReAct agent loop with context compaction`

---

### Task 6: Prompts, PR Manifest, Diff Batching, Specialists & Parsing (`crates/review`)

**Files:**
- Create: `crates/review/src/specialists.rs`
- Modify: `crates/review/src/prompt.rs`, `crates/review/src/parse.rs`, `crates/review/src/lib.rs`
- Test: `crates/review/src/prompt.rs`, `crates/review/src/parse.rs`, `crates/review/src/specialists.rs`

**Interfaces:**
- In `crates/review/src/specialists.rs`:
  - `#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)] pub enum SpecialistKind { Generalist, Bug, Security, Performance, Rules }`
  - `impl SpecialistKind`: `label(&self) -> &'static str`, `role_block(&self) -> &'static str`, `excludes_pure_style(&self) -> bool`.
  - `pub fn active_specialists(mode: ReviewMode, batch_has_rules: bool) -> Vec<SpecialistKind>`:
    - `Generalist` → `[Generalist]`
    - `Specialists` → `[Bug, Security, Performance]` + `Rules` if `batch_has_rules`
- In `crates/review/src/prompt.rs`:
  - `MAX_BATCH_CHARS: usize = 40_000`.
  - `ManifestEntry { path: String, old_path: Option<String>, status: FileStatus, added: usize, removed: usize, note: Option<&'static str> }`
  - `DiffBatch<'a> { files: Vec<&'a FileDiff>, rules: Vec<&'a Rule> }`
  - `pub fn build_manifest(all_files: &[FileDiff], ignored_paths: &HashSet<&str>, skipped_reasons: &HashMap<&str, &'static str>) -> String`
  - `pub fn batch_targets<'a>(targets: &'a [FileDiff], rules: &'a [Rule], max_chars: usize) -> Vec<DiffBatch<'a>>` (sorts by directory prefix / path, packs contiguous batches up to `max_chars`; single large file goes in solo batch).
  - `pub fn build_finder_system_prompt(kind: SpecialistKind, with_tools: bool) -> String` (static shared `<ReviewInstructions>` FIRST for prefix caching, `<Role>` at end!).
  - `pub fn build_finder_user_prompt(pr: &PullRequest, manifest: &str, batch: &DiffBatch<'_>, memories: &[Memory], repo_instructions: Option<&str>, with_tools: bool) -> String` (neutralizes closing tags on every untrusted block: `pr_title`, `pr_description`, `pr_manifest`, `rules`, `memories`, `repository_instructions`, `diffs`).
  - Keep `build_prompt` (or alias/retain unit compatibility if tests use it, or update `prompt.rs` tests cleanly).
  - Update `format_summary(counts: &BTreeMap<Severity, usize>, skipped: &[(String, &'static str)], ignored: usize, warnings: &[String]) -> String` to render `**Notes**\n- ...` when `!warnings.is_empty()`.
- In `crates/review/src/parse.rs`:
  - `pub fn submit_findings_tool_def() -> ToolDef` (JSON schema for `{"findings": [...]}`).
  - `pub fn parse_submit_findings(args: &serde_json::Value) -> Result<Vec<Suggestion>, String>` (reads `"findings"` array, parses per-element leniently like `parse_suggestions`).
  - `pub fn keep_valid_batch(suggestions: Vec<Suggestion>, allowed_by_file: &HashMap<String, BTreeSet<u32>>) -> Vec<Suggestion>` (filters to files in `allowed_by_file` and lines in that file's `BTreeSet<u32>`, normalizes paths, deduplicates exact `(file, line, title)`).

**Edge Cases & Pitfalls:**
- **Prefix-cache ordering:** Every specialist (`Bug`, `Security`, `Performance`, `Rules`, `Generalist`) must share **byte-identical** prefixes up through `</ReviewInstructions>` (and `</Workflow>`/`</Scope>`/`</UntrustedInput>`), with only `<Role>` differing at the end of the system prompt! Add a unit test asserting identical leading prefix bytes across all 4 specialists.
- **Self-contained fallback prompt (`with_tools = false`):** Drops ReAct tool workflow/instructions and asks directly for the JSON array (`[]` when clean) so non-tool OpenAI-compatible models still work smoothly.
- **Rules matching per batch:** Only include rules where `rule_matches_path(rule, &file.path)` is true for at least one file in that batch, and tag each rendered rule with its `path_glob` (e.g., `(applies to: src/**)` or `(all files)`) so the LLM does not misapply a `web/**` rule to a `crates/**` file in the same batch!
- **Prompt injection guard:** Neutralize `</repository_instructions>`, `</pr_manifest>`, `</diffs>`, etc., using `neutralize_close_tag`.

- [ ] **Step 1: Write failing tests**
  - `prompt.rs`:
    - `specialists_share_byte_identical_system_prefix`
    - `batch_targets_groups_directory_siblings_and_isolates_oversized_files`
    - `build_manifest_marks_ignored_and_skipped_files`
    - `finder_user_prompt_tags_rule_globs_and_neutralizes_repo_instructions`
    - `summary_renders_warnings_under_notes`
  - `parse.rs`:
    - `parse_submit_findings_extracts_valid_items_and_skips_incomplete`
    - `keep_valid_batch_filters_across_multiple_files_and_dedups`
  - `specialists.rs`: `active_specialists_omits_rules_when_none_match`.
- [ ] **Step 2:** Run `cargo test -j 2 -p adus-review` → FAIL.
- [ ] **Step 3:** Implement `specialists.rs`, `prompt.rs`, `parse.rs`, and update `lib.rs` exports.
- [ ] **Step 4:** Run `cargo test -j 2 -p adus-review` → PASS.
- [ ] **Step 5:** Commit `feat(review): add multi-file batching, PR manifest, and specialist prompts`

---

### Task 7: Per-Finding Tool-Enabled Verifier & Semantic Deduplication (`crates/review`)

**Files:**
- Create: `crates/review/src/verifier.rs`, `crates/review/src/dedup.rs`
- Modify: `crates/review/src/lib.rs`
- Test: `crates/review/src/verifier.rs` (`mod tests`), `crates/review/src/dedup.rs` (`mod tests`)

**Interfaces:**
- In `crates/review/src/verifier.rs`:
  - `pub const VERIFY_CONCURRENCY: usize = 4;`
  - `pub const VERIFY_MAX_STEPS: usize = 6;`
  - `pub const MAX_VERIFY_CANDIDATES: usize = 25;`
  - `CandidateFinding { pub suggestion: Suggestion, pub specialist: SpecialistKind }`
  - `VerificationReport { pub kept: Vec<CandidateFinding>, pub dropped_refuted: usize, pub dropped_unverified: usize, pub truncated_by_cap: usize, pub all_retryable_error: Option<ChatError> }`
  - `pub async fn verify_candidates(ctx: VerifyContext<'_>, candidates: Vec<CandidateFinding>, targets: &[FileDiff], rules: &[Rule], memories: &[Memory], repo_instructions: Option<&str>, toolbox: Option<&Toolbox>, deadline: tokio::time::Instant) -> VerificationReport`:
    - Each candidate's verification runs under `tokio::time::timeout_at(deadline, ...)`; a candidate cut by the deadline is dropped as unverified and is **not** a retryable failure (spec 4.11).
    - When `toolbox.is_some()`, runs per-finding `run_react_loop` (`VERIFY_MAX_STEPS = 6`) with `submit_verdict` tool (`{ keep: bool, rationale: String }`) + read-only tools.
    - When `toolbox.is_none()` (diff-only fallback), runs 1 single-turn `ChatRequest::text` per candidate against the annotated file diff + rules + memories, parsing `{"keep": bool, "rationale": ...}` from JSON.
- In `crates/review/src/dedup.rs`:
  - `pub struct DedupResult { pub kept: Vec<Suggestion>, pub dropped_duplicates: usize }`
  - `pub async fn deduplicate_findings(chat: &dyn ChatService, provider_id: Uuid, model: &str, usage_ctx: &UsageContext, findings: Vec<CandidateFinding>) -> DedupResult`:
    - When `findings.len() < 2`, returns unchanged immediately (0 LLM calls).
    - Otherwise makes 1 `ChatRequest::text` call asking for `{"keep_indices": [0, ...]}`, validates bounds (`< findings.len()`), deduplicates indices while preserving ascending order, rejects empty `keep_indices` (falling back safely to all findings), and on any `ChatError`/parse failure logs a warning and returns all `findings` unchanged.

**Edge Cases & Pitfalls:**
- **Severity prioritization before cap:** Sort stable descending by `Severity` (`Critical > Warning > Info`) before truncating at `MAX_VERIFY_CANDIDATES = 25`, recording `truncated_by_cap` so lower-priority info findings never crowd out criticals.
- **Evidence bundle per candidate:** Include the candidate JSON, the file's annotated patch hunk, plus (when `toolbox.is_some()`) an expanded ±40-line snippet around `candidate.suggestion.line` from `toolbox.read_file_snippet_sync(...)` so the Verifier often doesn't even need a tool call for local checks, while still having `read_file`/`grep` to verify callers & guards!
- **Rules / repo instructions awareness in Verifier:** Pass matching rules for that file and `repo_instructions`; tell the Verifier that if `specialist == SpecialistKind::Rules` (or a finding explicitly cites a team rule or repo instruction), do NOT drop it under the "pure style/formatting" exclusion—verify instead whether the rule applies to the file and whether the changed line breaks it.
- **Fail-closed semantics + retryable edge case (Review Focus #5):**
  - Individual candidate failure (`Invalid`, `Unparseable`, `ContextOverflow`, or `ToolsUnsupported`) drops that candidate and increments `dropped_unverified`.
  - If no candidate got a verdict (`kept.is_empty() && dropped_refuted == 0`), `dropped_unverified > 0`, and **every** verifier failure was a retryable chat error (`ChatError::AllKeysFailed`, `NoKeys`, `Internal`; a deadline cut is not), set `all_retryable_error` so `Engine` deletes the review row and lets the queue retry instead of silently posting `"No issues found."`.
- **Deterministic order preservation:** `buffer_unordered(VERIFY_CONCURRENCY)` returns outcomes in arbitrary order—tag each candidate with its index `0..N` and re-sort survivors by original index before dedup!

- [ ] **Step 1: Write failing tests**
  - `verifier.rs`:
    - `keeps_confirmed_and_drops_refuted_using_tools` (verifier calls `read_file`, then `submit_verdict(keep: false)` on one and `keep: true` on another).
    - `caps_at_25_prioritizing_critical_over_info`.
    - `fail_closed_drops_unparseable_and_invalid_verifier_outputs`.
    - `surfaces_retryable_error_when_all_candidates_fail_retryably`.
    - `deadline_drops_pending_candidates_as_unverified_not_retryable`.
    - `diff_only_fallback_verifies_without_tools`.
  - `dedup.rs`:
    - `skips_llm_call_for_zero_or_one_finding`.
    - `merges_duplicate_indices_and_preserves_order`.
    - `falls_back_safely_on_error_empty_or_out_of_bounds_indices`.
- [ ] **Step 2:** Run `cargo test -j 2 -p adus-review` → FAIL.
- [ ] **Step 3:** Implement `crates/review/src/verifier.rs` and `crates/review/src/dedup.rs`.
- [ ] **Step 4:** Run `cargo test -j 2 -p adus-review` → PASS.
- [ ] **Step 5:** Commit `feat(review): add per-finding tool-enabled Verifier and Semantic Dedup`

---

### Task 8: Wire Full Pipeline into `Engine`, Startup Sweep, `doctor` & `Dockerfile`

**Files:**
- Modify: `crates/review/src/engine.rs`, `crates/adus/src/main.rs`, `crates/server/src/doctor.rs`, `Dockerfile`, `crates/adus/tests/worker.rs`, `crates/review/tests/common/mod.rs`, `crates/review/tests/engine.rs`
- Test: `crates/review/tests/engine.rs`, `crates/server/tests/doctor.rs`

**Interfaces:**
- `Engine` in `crates/review/src/engine.rs`:
  - Add field `pub cloner: Arc<dyn RepoCloner>`.
  - `review_pr(&self, repository_id: Uuid, pr_number: i64, trigger: Trigger, mode_override: Option<ReviewMode>) -> Result<Option<Uuid>, EngineError>`.
- `crates/adus/src/main.rs`:
  - Call `adus_review::workspace::cleanup_orphaned_workspaces().await;` at startup in `serve()` before spawning workers.
  - Wire `cloner: Arc::new(GitCliCloner::default())` into `Engine`.
- `crates/server/src/doctor.rs`:
  - Add `check_git_binary(&mut extra).await` checking `git --version` (under `PROVIDER_TIMEOUT` / 5 s): `Ok` when exit 0 and version ≥ 2.31, `Status::Warn` when missing, failing or older (`"`git` 2.31+ not found — reviews fall back to diff-only mode without codebase context"`).
- `Dockerfile`:
  - Install `git` alongside `ca-certificates` in the runtime stage (`apt-get install -y --no-install-recommends ca-certificates git`).

**Edge Cases & Pitfalls:**
- **Clone attempt & fallback orchestration (Review Focus #4):**
  - Only clone when `git.clone_spec(&repo_ref, &pr)` returns `Some(spec)` AND `targets` is non-empty.
  - If `cloner.clone_pr(&spec).await` returns:
    - `Ok(ws)` → load `repo_instructions` + construct `Toolbox` with `Arc::new(ws)` and shared `Arc::new(ToolCache::default())`.
    - `Err(WorkspaceError::Unauthorized)` → mark connection `invalid` via `self.unauthorized(&conn).await` and return `Err(EngineError::Fatal(...))`.
    - `Err(WorkspaceError::TimedOut | TooLarge | Unavailable(_))` → log warning, push `"Codebase context unavailable — reviewed diff only"` to `warnings`, and proceed in diff-only fallback (`toolbox = None`).
- **Finder execution & `ToolsUnsupported` mid-run fallback (Review Focus #4 & #6):**
  - Resolve mode: `mode_override.or(repo.review_mode).unwrap_or(settings.review_mode)`.
  - Run batches up to `BATCH_CONCURRENCY = 2` at a time; within a batch run all of its specialists in parallel (spec 4.7). Keep results ordered by (batch index, specialist order).
  - **Stage deadlines (spec 4.11, Review Focus #9):** compute `start = Instant::now()` at the top of `run()`; Finder deadline `start + review_timeout * 60 / 100`, Verifier `* 90 / 100`, Dedup `* 97 / 100`. Wrap each Finder task in `timeout_at`; a cut task is a failed specialist with warning `"{label} reviewer did not finish in time"`; if no Finder task finished at all → `Fatal("review timed out")`. Pass the Verifier deadline to `verify_candidates`; wrap `deduplicate_findings` in `timeout_at` and keep the verified list on expiry.
  - If any task returns `AgentError::ToolsUnsupported(_)`: log warning, add `"Model does not support tool calling — reviewed diff only"` to `warnings`, and re-run the Finder in self-contained diff-only mode (`with_tools = false`) — discarding the partial tool-mode attempt!
  - Partial specialist failures (when ≥ 1 specialist task succeeded): log warning, add `"{label} reviewer did not complete"` to `warnings`, and continue with surviving candidates.
  - All tasks failed: if all errors are retryable (`AllKeysFailed`, `NoKeys`, `Internal`), return `Err(chat_error(...))` so the job retries (`Retryable`); if `Invalid` / `Unparseable`, return `Err(EngineError::Fatal(...))` (if `Unparseable` produced partial `skipped`, handle cleanly — e.g., if `all` batches were `Unparseable`, mark `skipped` `unparseable` in summary like the current engine when possible, or fail `Fatal` cleanly without posting an empty broken review).
- **Empty targets + skipped files:** Preserve current behavior! If `targets.is_empty() && !skipped.is_empty()` (e.g., all changed files were `too_large` or `file_limit`), skip clone + LLM entirely and post the summary with `**Skipped files**`. If `targets.is_empty() && skipped.is_empty()`, return `Outcome::Skipped("nothing_to_review")`.
- **Verifier & Dedup integration (Review Focus #5):**
  - Run `keep_valid_batch` across finder outputs → map to `Vec<CandidateFinding>` → run `verify_candidates`.
  - If `verification.all_retryable_error` is `Some(err)` → `return Err(chat_error(err))`.
  - Add warnings for `dropped_unverified` (`"{n} candidate finding(s) could not be verified and were omitted"`) and `truncated_by_cap`.
  - Run `deduplicate_findings` on kept findings.
  - Build `InlineComment`s + `format_summary(&counts, &skipped, ignored, &warnings)` → `git.post_review(...)`.
- **RAII cleanup on timeout (Review Focus #8):** Ensure `RepoWorkspace` lives inside the `run()` future passed to `tokio::time::timeout(self.review_timeout, run)` so timeout or `JoinHandle::abort()` drops `RepoWorkspace` immediately.

- [ ] **Step 1: Update test fakes (`crates/review/tests/common/mod.rs`, `crates/adus/tests/worker.rs`) & write failing integration tests in `crates/review/tests/engine.rs`**
  - Update `FakeChat` in `crates/review/tests/common/mod.rs` to inspect/respond to multi-turn `ChatRequest` (`messages`, `tools`) and keep helper methods so existing engine tests (adapted for Finder → Verifier → Dedup flow) stay readable.
  - Add `FakeCloner` over a `TempDir` repo fixture.
  - New tests in `crates/review/tests/engine.rs`:
    - `agentic_generalist_uses_tools_verifies_and_dedups` (Finder reads helper file, emits 2 findings; Verifier refutes 1 and keeps 1; 1 comment posted).
    - `specialists_mode_runs_bug_security_performance_and_rules_and_dedups` (Bug & Security both flag same flaw → Verifier keeps both → Dedup merges to 1).
    - `partial_specialist_failure_publishes_surviving_with_warning`.
    - `clone_failure_falls_back_to_diff_only_with_summary_note`.
    - `clone_unauthorized_marks_connection_invalid_and_fails`.
    - `tools_unsupported_falls_back_to_diff_only_with_summary_note`.
    - `verifier_fail_closed_drops_unverified_finding_with_warning`.
    - `repo_instructions_loaded_from_clone_into_prompt`.
    - `review_mode_precedence_command_overrides_repo_overrides_settings`.
    - `finder_deadline_cuts_slow_specialist_and_posts_survivors_with_note`.
  - Add `doctor` test for `git` binary check in `crates/server/tests/doctor.rs`.
- [ ] **Step 2:** Run `cargo test -j 2 -p adus-review -p adus -p adus-server` → FAIL.
- [ ] **Step 3:** Implement `crates/review/src/engine.rs`, `crates/adus/src/main.rs`, `crates/server/src/doctor.rs`, `Dockerfile`.
- [ ] **Step 4:** Run `cargo test -j 2 -p adus-review -p adus -p adus-server` → PASS (with zero skipped/relaxed assertions).
- [ ] **Step 5:** Commit `feat(review): wire agentic workspace, specialists, Verifier, and Dedup into Engine`

---

### Task 9: Server API for `review_mode` (`Settings` & `Repositories`)

**Files:**
- Modify: `crates/server/src/api/settings.rs`, `crates/server/src/api/repositories.rs`
- Test: `crates/server/tests/settings_ignore.rs` (or new `crates/server/tests/review_mode.rs`), `crates/server/tests/connections.rs`

**Interfaces:**
- `GET /api/v1/settings` & `PUT /api/v1/settings`:
  - `SettingsJson` adds `review_mode: ReviewMode`.
  - `SettingsBody` adds `review_mode: Option<ReviewMode>` (when omitted/absent, keeps `current.review_mode` so existing `PUT /settings` requests without `review_mode` remain 100% backwards compatible!).
- `GET /api/v1/repositories` & `PATCH /api/v1/repositories/{id}`:
  - `RepositoryJson` adds `review_mode: Option<ReviewMode>`.
  - `UpdateRepository` adds `#[serde(default, deserialize_with = "present_or_null")] review_mode: Option<Option<ReviewMode>>`.
  - Invalid enum value (e.g., `"turbo"`) → standard `ApiJson` 422 `validation_error`.

**Edge Cases & Pitfalls:**
- **Role enforcement:** Non-admin `user` role receives 403 on `PUT /settings` and `PATCH /repositories/{id}`, and can read `review_mode` on `GET`.
- **Null clearing on repository PATCH:** Sending `{"review_mode": null}` resets repo override back to `None` (inherit global default), while omitting `review_mode` leaves it unchanged.

- [ ] **Step 1: Write failing tests** (`crates/server/tests/review_mode.rs`):
  - `get_settings_defaults_to_generalist_and_put_updates_it`
  - `put_settings_omitting_review_mode_keeps_existing_value`
  - `patch_repo_sets_and_clears_review_mode_override`
  - `invalid_review_mode_returns_422`
  - `user_role_cannot_change_review_mode_403`
- [ ] **Step 2:** Run `cargo test -j 2 -p adus-server --test review_mode --test settings_ignore --test connections` → FAIL.
- [ ] **Step 3:** Implement `crates/server/src/api/settings.rs` and `crates/server/src/api/repositories.rs`.
- [ ] **Step 4:** Run `cargo test -j 2 -p adus-server` → PASS.
- [ ] **Step 5:** Commit `feat(server): expose review_mode in settings and repositories APIs`

---

### Task 10: Dashboard UI for `review_mode` & Final Workspace Verification

**Files:**
- Modify: `web/src/routes/settings/+page.svelte`, `web/src/routes/repositories/+page.svelte`

**Interfaces:**
- Consumes: `review_mode` on `/api/v1/settings` and `/api/v1/repositories[/{id}]`.
- `web/src/routes/settings/+page.svelte`:
  - Extend `Settings` type with `review_mode: 'generalist' | 'specialists'`.
  - Add a **Review mode** `<Select.Root>` field in the Review settings form (`Generalist — balanced single-pass review` / `Specialists — parallel Bug, Security, Performance & Rules`), plus helper text noting `@adudu review deep` runs specialists on any PR.
  - Send `review_mode` in `PUT /settings`.
- `web/src/routes/repositories/+page.svelte`:
  - Extend `Repo` type with `review_mode: 'generalist' | 'specialists' | null`.
  - Update both the loading `TableSkeleton` headers and the `<Table.Header>`/`<Table.Body>` to add a **Review mode** column (`Default (global)`, `Generalist`, `Specialists`) — interactive `<Select>` when `isAdmin`, muted text label when non-admin `user`. Update empty-row `colspan={6}`.

**Edge Cases & Pitfalls:**
- Follow `DESIGN.md` tokens and `#lib/components/ui/...` imports (`#lib/api.js`, `#lib/session.svelte.js`, no `$lib`).
- Remember `pnpm -C web build` MUST run before `cargo test --workspace` because `crates/server/src/spa.rs` embeds `web/build`!
- No browser automation (`capture_browser_scenario` hangs on Windows per `AGENTS.md`); verify with `pnpm -C web fmt`, `pnpm -C web check`, `pnpm -C web build`, and curl / code inspection.

- [ ] **Step 1:** Update `web/src/routes/settings/+page.svelte` and `web/src/routes/repositories/+page.svelte`.
- [ ] **Step 2:** Run `pnpm -C web fmt && pnpm -C web check && pnpm -C web build` → 0 errors.
- [ ] **Step 3:** Run full Rust verification:
  - `cargo fmt --all -- --check`
  - `cargo clippy --workspace --all-targets -- -D warnings`
  - `cargo test --workspace -j 2`
- [ ] **Step 4:** Run `git status` and review full diff against the **Review Focus** checklist above.
- [ ] **Step 5:** Commit `feat(web): add review_mode controls to settings and repositories pages`
