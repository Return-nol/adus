# Provider Model Tiers (`primary_model` / Optional `fast_model`), Interactive Model Discovery & Reasoning Effort Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move model configuration onto each AI Provider (`primary_model` required, `fast_model` optional falling back to `primary_model`), add live model discovery (`LlmClient::list_models` + searchable Combobox unlocked after testing the provider API connection), drop global/per-repo raw model strings (`default_model` / `review_model`), and add auto-adjusted `ReasoningEffort` (`Low | Medium | High`) with transparent non-reasoning model fallback.

**Architecture:** `adus-llm` adds `LlmClient::list_models` (fetching `/models`, `/v1/models`, and `/v1beta/models`) and `ReasoningEffort` (`Low`, `Medium`, `High`) on `ChatRequest`, mapping effort to OpenAI `reasoning_effort`, Anthropic `thinking` (with multi-turn thinking block + signature echo via `ToolCall.provider_meta`), and Gemini `thinkingConfig.thinkingBudget` (with `thought: true` part filtering), and transparently retrying once with `effort: None` when a model returns HTTP 400/422 rejecting reasoning parameters. `adus-db` (migration `0008_review_mode_and_provider_models.sql`) adds `primary_model TEXT NOT NULL` and `fast_model TEXT NULL` (`AiProvider::effective_fast_model`) to `ai_providers` and drops `settings.default_model` and `repositories.review_model`. `adus-review` resolves the provider's model tiers and task effort, while `adus-server` and `web/` expose `POST /api/v1/ai-providers/models`, interactive Combobox model selection gated on connection testing, optional initial key creation on provider creation, 1-click key testing, and per-repo provider override.

**Post-review note (2026-11-21):** the migration was folded into `0008_review_mode_and_provider_models.sql`, and `primary_model` became nullable (`NULL` = needs setup, never an invented model). The spec records this and the other review fixes (reasoning output headroom, 300 s timeout with effort, HMAC-keyed model-list cache, stored keys only sent to the saved base URL); where this plan's text differs, the spec wins.

**Tech Stack:** Rust 2024 (`tokio`, `reqwest`, `serde`, `serde_json`, `sqlx` runtime queries, `thiserror`), Postgres, Redis, SvelteKit 3 + Svelte 5 + shadcn-svelte (`Popover` + `Command` combobox, `DESIGN.md`).

**Spec:** [2026-11-21-provider-models-and-reasoning-design.md](../specs/2026-11-21-provider-models-and-reasoning-design.md) (also binding: [2026-11-19-adus-design.md](../specs/2026-11-19-adus-design.md), `AGENTS.md`, `DESIGN.md`).

---

## Global Constraints

- **Scope isolation:** This plan covers **only** Provider Model Tiers (`primary_model` + optional `fast_model`), `ReasoningEffort`, `list_models` / `POST /api/v1/ai-providers/models`, and the corresponding DB, `Engine`, `doctor`, REST API, and Dashboard UI changes. It does **not** include the remaining agentic review stages (`specialists`, `verifier`, `dedup`) from [2026-11-21-agentic-review-engine.md](2026-11-21-agentic-review-engine.md).
- **AGENTS.md baseline:** Runtime `sqlx::query` / `sqlx::query_as` only (never `query!` macros); `ApiJson<T>` / `ApiPath<T>` / `ApiQuery<T>` and `ApiError` JSON errors in `crates/server`; `#lib/*.js` / `#lib/*.svelte` imports and `DESIGN.md` tokens in `web/`; no browser automation; `cargo test --workspace -j 2` on Windows (and clean `target/debug/incremental` if disk space drops).
- **Clean-room and no-assumption rule (binding, verbatim from `AGENTS.md`):**
  1. **Kodus community code** (https://github.com/kodustech/kodus-ai, anything outside `libs/ee/**` and not named `*.ee.*`) may be read as a behaviour reference and inspiration. Never port or translate it line by line: Adus code, prompts and schemas are written fresh. **Kodus enterprise code** (`libs/ee/**`, `*.ee.*`) is never opened, fetched or searched, not even to look.
  2. **Order of authority:** the task brief, then the spec, then this file and the existing Adus code. Kodus community behaviour (or public provider/git docs for wire formats) only fills a gap none of those settles. Never guess from memory of how Kodus or another tool works; if a gap remains, raise it instead of assuming.
  3. **Openness:** every implementer report lists its assumptions under an "Assumptions" heading and cites each Kodus community file consulted (path) and the behaviour taken from it.
  4. **Reviewers** flag as Important any line-by-line port or translation, anything that looks derived from Kodus enterprise code, uncited Kodus use, and unstated assumptions beyond the brief/spec.
  5. **Plans and prompts:** every new plan repeats this rule in its Global Constraints, and every implementer and reviewer subagent prompt carries it verbatim.
- **Rust Best Practices:**
  - **Borrowing & ownership:** Take `&str`, `&[T]`, `Cow<'_, str>` at boundaries; pass `Copy` enums (`ReviewMode`, `Severity`, `ReasoningEffort`) by value; avoid `.clone()` inside hot loops.
  - **Type-driven invariants & error handling:** Typed `thiserror` error enums per boundary; zero `.unwrap()`/`.expect()` outside tests; concrete `Deserialize` structs for model-list payloads.
  - **Dispatch & linting:** Static dispatch / closed enums internally; `dyn Trait` only at external/test seams (`GitProvider`, `GitProviderFactory`, `LlmClient`, `ChatService`, `RepoCloner`, `JobRunner`). `cargo clippy --workspace --all-targets -- -D warnings` must pass cleanly.
- **Crate acyclicity:** Keep `core` < (`db`, `git`, `queue`), (`core`, `db`) < `llm`, (`core`, `db`, `git`, `llm`, `queue`) < `review`, and (`server`, `adus`) on top.
- **Model choice for subagents:** Task 1 touches `crates/llm` wire protocols → **opus, effort high**. Tasks 2, 3, 4, 5 → **sonnet, effort high**.

---

## Review Focus

1. **Non-thinking model rejects reasoning parameters with HTTP 400/422:** A provider pairs a reasoning `primary_model` (e.g. `o3`) with a non-reasoning `fast_model` (e.g. `gpt-4o-mini` or an Ollama model) that returns 400 `"Unrecognized request argument supplied: reasoning_effort"` (or Anthropic/Gemini rejecting `thinking` / `thinkingConfig`). `LlmClient::chat` must retry once with `effort: None` and succeed without marking the key invalid or logging a failed usage row. Pinned in Task 1 (`retries_without_effort_when_provider_rejects_reasoning_param`).
2. **Anthropic multi-turn tool call with extended thinking enabled:** Anthropic `/v1/messages` returns a `thinking` block (with `signature`) followed by a `tool_use` block, and rejects the next turn with HTTP 400 unless the assistant message replays the `thinking` (and `redacted_thinking`) blocks verbatim before the `tool_use` block. Pinned in Task 1 (`anthropic_sends_thinking_budget_and_replays_thinking_blocks_on_tool_turns`).
3. **Gemini thinking parts polluting JSON output & `models/` prefix in `list_models`:** Gemini 2.5/3 returns `parts: [{"text": "internal chain of thought...", "thought": true}, {"text": "[{\"file\": ...}]"}]` and `/v1beta/models` returns `"name": "models/gemini-2.5-pro"` alongside embedding-only models. `GeminiClient` must exclude `thought: true` parts from `ChatResponse.text` and `list_models` must strip `"models/"` and filter to `generateContent`-capable models. Pinned in Task 1 (`gemini_sends_thinking_budget_and_filters_thought_parts` and `list_models_across_openai_anthropic_and_gemini`).
4. **Optional `fast_model` fallback & upgrade migration:** An existing DB has `ai_providers` rows plus `settings.default_model` or `repositories.review_model`. Migration `0008_review_mode_and_provider_models.sql` must backfill `primary_model` (`NOT NULL`), keep `fast_model` nullable (`TEXT NULL`), and `AiProvider::effective_fast_model()` must return `primary_model` whenever `fast_model` is `None` or blank. Pinned in Task 2 (`provider_models_crud_optional_fast_model_and_effective_fallback`).
5. **Model discovery auth failure & disabled provider handling:** `POST /api/v1/ai-providers/models` with a rejected key must return 422 `token_invalid` without leaking the key or URL, and when editing a provider with `provider_id` it must decrypt and use a stored key on the server. A disabled default provider must be treated as `no_model` by `Engine` and `Status::Fail` by `doctor`. Pinned in Task 3 (`disabled_provider_treated_as_no_model`) and Task 4 (`discover_models_with_inline_key_stored_provider_key_and_422_on_bad_token`).

---

## Task Overview

| # | Scope | Primary Crates / Files |
|---|---|---|
| 1 | `ReasoningEffort` (`Low \| Medium \| High`), `LlmClient::list_models`, per-provider wire mapping, 1-shot non-reasoning fallback, `AgentLlm.effort` | `crates/llm`, `crates/review/src/agent.rs` |
| 2 | Migration `0008_review_mode_and_provider_models.sql` (`primary_model` NOT NULL & `fast_model` NULL on `ai_providers`, drop `default_model` & `review_model`) & `crates/db` | `migrations/0008_review_mode_and_provider_models.sql`, `crates/db/src/{ai,settings,repositories}.rs`, `crates/llm/tests/router.rs` |
| 3 | Update `Engine` to resolve provider `primary_model` / `effective_fast_model()` & auto-adjusted `ReasoningEffort` | `crates/review/src/engine.rs`, `crates/review/tests/` |
| 4 | Server REST APIs (`POST /ai-providers/models`, `ai`, `settings`, `repositories`) & `doctor` (`Default provider` check) | `crates/server/src/{api/*,doctor.rs}`, `crates/server/tests/` |
| 5 | Dashboard UI (interactive Combobox in `provider-dialog.svelte` gated on connection test, 1-click key test in `ai-providers`, provider selection in `settings` & `repositories`) | `web/src/lib/components/provider-dialog.svelte`, `web/src/routes/{ai-providers,settings,repositories}/+page.svelte` |

---

### Task 1: `ReasoningEffort`, `LlmClient::list_models` & Per-Provider Wire Support (`crates/llm`, `crates/review/src/agent.rs`)

**Files:**
- Modify: `crates/llm/src/types.rs`, `crates/llm/src/lib.rs`, `crates/llm/src/openai.rs`, `crates/llm/src/anthropic.rs`, `crates/llm/src/gemini.rs`, `crates/review/src/agent.rs`
- Test: `crates/llm/tests/clients.rs`, `crates/review/src/agent.rs`

**Interfaces:**
- Produces in `adus_llm::types` (and re-exported in `adus_llm`):
  - `#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)] #[serde(rename_all = "snake_case")] pub enum ReasoningEffort { Low, Medium, High }`
  - `impl ReasoningEffort { pub fn as_str(self) -> &'static str }` (`"low"`, `"medium"`, `"high"`).
  - `ChatRequest` adds `pub effort: Option<ReasoningEffort>`.
  - `ChatRequest::text(...)` initializes `effort: None`.
  - `ChatRequest::with_effort(mut self, effort: ReasoningEffort) -> Self` sets `self.effort = Some(effort)`.
  - `LlmClient` trait adds `async fn list_models(&self, api_key: &str) -> Result<Vec<String>, LlmError>;`.
  - `pub(crate) fn is_reasoning_unsupported(err: &LlmError) -> bool` (returns `true` when `err` is `LlmError::Invalid(msg)` whose lowercased text matches reasoning/thinking rejection markers: `"reasoning_effort"`, `"reasoning effort"`, `"thinking"`, `"thinking_config"`, `"thinkingconfig"`, `"thinking_budget"`, `"thinkingbudget"`, `"budget_tokens"`, `"does not support reasoning"`).
- Produces in `crates/review/src/agent.rs`:
  - `AgentLlm<'a>` adds `pub effort: Option<ReasoningEffort>` and passes `effort: llm.effort` into `ChatRequest` inside `request()`.

- [ ] **Step 1: Write failing tests in `crates/llm/tests/clients.rs` and `crates/review/src/agent.rs`**
  - In `crates/llm/tests/clients.rs`:
    - `openai_sends_reasoning_effort_when_configured`: `ChatRequest::text("o3", "sys", "hi", 100).with_effort(ReasoningEffort::High)` sends `"reasoning_effort": "high"`.
    - `retries_without_effort_when_provider_rejects_reasoning_param` (Review Focus #1): first mock POST with `reasoning_effort` returns HTTP 400 `{"error": {"message": "Unrecognized request argument supplied: reasoning_effort"}}`; client retries once without `reasoning_effort` and returns `Ok(ChatResponse)`.
    - `anthropic_sends_thinking_budget_and_replays_thinking_blocks_on_tool_turns` (Review Focus #2): first request with `ReasoningEffort::Medium` sends `"thinking": {"type": "enabled", "budget_tokens": 4096}` and `"max_tokens": 8192 + 4096`; mock responds with `content: [{"type": "thinking", "thinking": "check caller", "signature": "sig_1"}, {"type": "redacted_thinking", "data": "red_1"}, {"type": "tool_use", "id": "tu_1", "name": "read_file", "input": {"path": "a.rs"}}]`; second turn echoes the `Assistant` message with those `tool_calls` and verifies the outgoing Anthropic request replays the `thinking` (with `signature: "sig_1"`) and `redacted_thinking` blocks before the `tool_use` block in the assistant message's `content`.
    - `gemini_sends_thinking_budget_and_filters_thought_parts` (Review Focus #3): request with `ReasoningEffort::Low` sends `"generationConfig": {"maxOutputTokens": 200, "thinkingConfig": {"thinkingBudget": 1024}}`; mock returns `parts: [{"text": "internal thought", "thought": true}, {"text": "visible answer"}]`; asserts `resp.text == "visible answer"`.
    - `list_models_across_openai_anthropic_and_gemini` (Review Focus #3):
      - OpenAI `GET /models` returning `{"data": [{"id": "gpt-5"}, {"id": "gpt-5-mini"}]}` → `vec!["gpt-5", "gpt-5-mini"]`.
      - Anthropic `GET /v1/models` returning `{"data": [{"id": "claude-sonnet-4-5"}, {"id": "claude-haiku-4-5"}]}` → `vec!["claude-haiku-4-5", "claude-sonnet-4-5"]`.
      - Gemini `GET /v1beta/models` returning `{"models": [{"name": "models/gemini-2.5-pro", "supportedGenerationMethods": ["generateContent"]}, {"name": "models/text-embedding-004", "supportedGenerationMethods": ["embedContent"]}]}` → `vec!["gemini-2.5-pro"]` (strips `"models/"` and filters out embedding-only models).
  - In `crates/review/src/agent.rs` (`mod tests`):
    - Update `AgentLlm` test helper to set `effort: Some(ReasoningEffort::Medium)` and assert `FakeChat` receives `req.effort == Some(ReasoningEffort::Medium)`.

- [ ] **Step 2: Run tests to verify they fail**
  - Run: `cargo test -j 2 -p adus-llm -p adus-review`
  - Expected: FAIL (`ReasoningEffort`, `ChatRequest.effort`, and `LlmClient::list_models` not defined).

- [ ] **Step 3: Implement `ReasoningEffort`, `list_models`, wire mappings, 1-shot retry fallback, and `AgentLlm.effort`**
  - Implement in `crates/llm/src/{types,lib,openai,anthropic,gemini}.rs` and `crates/review/src/agent.rs` as specified in spec Section 3.2 and 3.3.

- [ ] **Step 4: Run tests to verify they pass**
  - Run: `cargo test -j 2 -p adus-llm -p adus-review`
  - Expected: PASS

- [ ] **Step 5: Commit**
  - `git add crates/llm crates/review/src/agent.rs`
  - `git commit -m "feat(llm): add ReasoningEffort, list_models, and provider thinking support"`

---

### Task 2: Migration `0009` (`primary_model` & Optional `fast_model` on `ai_providers`; Drop `default_model` & `review_model`) & `crates/db`

**Files:**
- Create: `migrations/0008_review_mode_and_provider_models.sql`
- Modify: `crates/db/src/ai.rs`, `crates/db/src/settings.rs`, `crates/db/src/repositories.rs`, `crates/llm/tests/router.rs`
- Test: `crates/db/src/ai.rs`, `crates/db/src/repositories.rs`, `crates/llm/tests/router.rs`

**Interfaces:**
- Produces in `crates/db/src/ai.rs`:
  - `AiProvider` adds `pub primary_model: String` and `pub fast_model: Option<String>`.
  - `impl AiProvider { pub fn effective_fast_model(&self) -> &str }` (returns `self.fast_model.as_deref().map(str::trim).filter(|m| !m.is_empty()).unwrap_or(&self.primary_model)`).
  - `pub async fn insert_provider(pool: &PgPool, kind: ProviderKind, name: &str, base_url: Option<&str>, primary_model: &str, fast_model: Option<&str>) -> Result<AiProvider>`
  - `pub async fn update_provider(pool: &PgPool, id: Uuid, name: &str, base_url: Option<&str>, primary_model: &str, fast_model: Option<&str>, enabled: bool) -> Result<Option<AiProvider>>`
- Produces in `crates/db/src/settings.rs`:
  - `Settings` drops `pub default_model: Option<String>` (retains `default_provider_id: Option<Uuid>`, `max_files_per_review: i32`, `max_patch_lines_per_file: i32`, `ignore_paths: Option<Vec<String>>`, `ignored_title_keywords: Vec<String>`, `review_mode: ReviewMode`).
- Produces in `crates/db/src/repositories.rs`:
  - `Repository` and `RepoPatch` drop `review_model` (retain `review_provider_id` and `review_mode`).

- [ ] **Step 1: Write failing DB tests**
  - In `crates/db/src/ai.rs` (`#[cfg(test)] mod tests`):
    - `provider_models_crud_optional_fast_model_and_effective_fallback` (Review Focus #4): inserts a provider with `primary_model = "claude-sonnet-4-5"` and `fast_model = None`, asserts `provider.effective_fast_model() == "claude-sonnet-4-5"`; updates `fast_model` to `Some("claude-haiku-4-5")`, asserts `provider.effective_fast_model() == "claude-haiku-4-5"`; updates `fast_model` back to `None`, asserts `provider.fast_model.is_none()`.
  - Update `crates/db/src/repositories.rs` test fixtures and `crates/llm/tests/router.rs` (`insert_provider`, `update_provider`, `Settings`) to the new signatures.

- [ ] **Step 2: Run tests to verify they fail**
  - Run: `cargo test -j 2 -p adus-db -p adus-llm`
  - Expected: FAIL

- [ ] **Step 3: Create `migrations/0008_review_mode_and_provider_models.sql` and update `crates/db/src/{ai,settings,repositories}.rs`**
  - Implement the SQL from spec Section 3.1 in `migrations/0008_review_mode_and_provider_models.sql`.
  - Update `AiProvider`, `PROVIDER_COLUMNS`, `insert_provider`, and `update_provider` in `crates/db/src/ai.rs`.
  - Remove `default_model` from `crates/db/src/settings.rs` and `review_model` from `crates/db/src/repositories.rs`.

- [ ] **Step 4: Run tests to verify they pass**
  - Run: `cargo test -j 2 -p adus-db -p adus-llm`
  - Expected: PASS

- [ ] **Step 5: Commit**
  - `git add migrations/0008_review_mode_and_provider_models.sql crates/db crates/llm/tests/router.rs`
  - `git commit -m "feat(db): add primary_model and optional fast_model to ai_providers"`

---

### Task 3: Update `Engine` to Resolve Provider Model Tiers & Auto-Adjusted `ReasoningEffort` (`crates/review`)

**Files:**
- Modify: `crates/review/src/engine.rs`, `crates/review/tests/common/mod.rs`, `crates/review/tests/engine.rs`
- Test: `crates/review/tests/engine.rs`

**Interfaces:**
- Consumes: `AiProvider` (`primary_model`, `fast_model`, `effective_fast_model()`), `Settings` (`default_provider_id`, `review_mode`), `Repository` (`review_provider_id`, `review_mode`) from `adus_db`; `ReasoningEffort` from `adus_llm`.
- Produces in `crates/review/src/engine.rs`:
  - `pub fn finder_effort(mode: ReviewMode) -> ReasoningEffort` (`ReviewMode::Generalist` → `ReasoningEffort::Medium`, `ReviewMode::Specialists` → `ReasoningEffort::High`).
  - Provider & model resolution in `Engine::run`:
    - `let provider_id = repo.review_provider_id.or(settings.default_provider_id);`
    - Load `provider` via `adus_db::ai::get_provider(&self.db, id).await.map_err(db_error)?`, requiring `provider.enabled && !provider.primary_model.trim().is_empty()`.
    - If missing, disabled, or `primary_model` is blank → existing `no_model` path (`Skipped("no_model")` on `Trigger::Updated`, no-model PR comment + `Fatal("no review model configured")` on `Trigger::Opened | Trigger::Command`).
  - Mode & effort resolution:
    - `let mode = mode_override.or(repo.review_mode).unwrap_or(settings.review_mode);`
    - `let effort = finder_effort(mode);`
    - Pass `(&provider.primary_model, effort)` into `self.ask(...)` (which constructs `ChatRequest::text(model, system, user, MAX_OUTPUT_TOKENS).with_effort(effort)`).

- [ ] **Step 1: Update `crates/review/tests/common/mod.rs` and write failing tests in `crates/review/tests/engine.rs`**
  - In `crates/review/tests/common/mod.rs`:
    - Add `pub effort: Option<ReasoningEffort>` to `RecordedCall` in `FakeChat`.
    - Update `setup()` to pass `"gpt-test"` and `Some("gpt-fast")` to `ai::insert_provider` and remove `current.default_model = ...`.
  - In `crates/review/tests/engine.rs`:
    - Update `repo_model_override_wins` (`repo_provider_override_wins`): sets `review_provider_id: Some(Some(other.id))` on the repository and asserts `calls[0].provider_id == other.id` and `calls[0].model == "claude-test"`.
    - Add `disabled_provider_treated_as_no_model` (Review Focus #5): disabling the configured `AiProvider` returns `Skipped("no_model")` on `Updated` and posts the no-model comment + `Fatal("no review model configured")` on `Opened`.
    - Add `review_mode_adjusts_reasoning_effort`: verifies `Generalist` sends `Some(ReasoningEffort::Medium)` and `Specialists` (via `mode_override`, `repo.review_mode`, or `settings.review_mode`) sends `Some(ReasoningEffort::High)`.

- [ ] **Step 2: Run tests to verify they fail**
  - Run: `cargo test -j 2 -p adus-review`
  - Expected: FAIL

- [ ] **Step 3: Implement provider model resolution and `finder_effort` in `crates/review/src/engine.rs`**
  - Update `Engine::review_pr` to forward `mode_override` into `self.run(repo, review_id, pr_number, trigger, mode_override)`.
  - Resolve `AiProvider` via `ai::get_provider`, validate `enabled && !primary_model.trim().is_empty()`, compute `effort = finder_effort(mode)`, and attach `.with_effort(effort)` in `Engine::ask`.

- [ ] **Step 4: Run tests to verify they pass**
  - Run: `cargo test -j 2 -p adus-review`
  - Expected: PASS

- [ ] **Step 5: Commit**
  - `git add crates/review`
  - `git commit -m "feat(review): resolve provider primary_model and auto-adjust reasoning effort"`

---

### Task 4: Server REST APIs (`POST /ai-providers/models`, `ai`, `settings`, `repositories`) & `doctor` (`crates/server`)

**Files:**
- Modify: `crates/server/src/api/ai.rs`, `crates/server/src/api/settings.rs`, `crates/server/src/api/repositories.rs`, `crates/server/src/doctor.rs`
- Test: `crates/server/tests/ai.rs`, `crates/server/tests/settings_ignore.rs`, `crates/server/tests/connections.rs`, `crates/server/tests/doctor.rs`, `crates/server/tests/usage.rs`

**Interfaces:**
- In `crates/server/src/api/ai.rs`:
  - Route `.route("/ai-providers/models", post(discover_models))` (admin-only):
    - `DiscoverModels { kind: Option<ProviderKind>, base_url: Option<String>, key: Option<String>, provider_id: Option<Uuid> }`.
    - Resolves `kind` and `base_url` (from `provider_id` if given, else from `body.kind` and `body.base_url`), resolves `secret` (inline `body.key` if non-empty, else first usable/enabled key of `provider_id`), calls `client_for(...).list_models(&secret).await`, maps `LlmError::Unauthorized` → 422 `token_invalid`, other errors → 502 `provider_error`, and returns `200 OK` `{"models": Vec<String>}`.
  - `CreateProvider`: `{ kind: ProviderKind, name: String, base_url: Option<String>, primary_model: String, fast_model: Option<String>, key: Option<String>, key_label: Option<String> }` (`primary_model` required via `required(..., "primary_model")`; `fast_model` trimmed, empty string becomes `None`; when `key` is non-empty after trim, inserts initial `AiKey` with `key_label` defaulting to `"Default"` and includes it in `ProviderJson.keys`).
  - `UpdateProvider`: `{ name: Option<String>, #[serde(default, deserialize_with = "present_or_null")] base_url: Option<Option<String>>, primary_model: Option<String>, #[serde(default, deserialize_with = "present_or_null")] fast_model: Option<Option<String>>, enabled: Option<bool> }`.
  - `ProviderJson`: includes `primary_model: String` and `fast_model: Option<String>`.
  - `TestKey`: `{ #[serde(default)] model: Option<String> }`; when `model` is omitted or blank, defaults to `provider.effective_fast_model().to_string()`.
- In `crates/server/src/api/settings.rs`:
  - `SettingsJson` & `SettingsBody`: drop `default_model`; when `default_provider_id` is `Some(id)`, verify `ai::get_provider(&state.db, id)` exists (else 404 `not_found`).
- In `crates/server/src/api/repositories.rs`:
  - `RepositoryJson` & `UpdateRepository`: drop `review_model`; keep `review_provider_id` (validating `ai::get_provider` exists when `Some(Some(id))`, else 404 `not_found`).
- In `crates/server/src/doctor.rs`:
  - `check_settings`: checks `"Default provider"` (`settings.default_provider_id` points to an enabled `AiProvider` with non-empty `primary_model`; formats `"Default provider: {name} ({primary_model} / {fast_model})"` when `fast_model` is `Some`, or `"Default provider: {name} ({primary_model})"` when `None`) + `"AI key"`.

- [ ] **Step 1: Update existing server tests and add new failing API & doctor tests**
  - In `crates/server/tests/ai.rs`:
    - Update `openai_provider` helper to include `"primary_model": "gpt-5"`.
    - Add `provider_requires_primary_model_allows_optional_fast_model_and_initial_key`: verifies 422 when `primary_model` is blank, verifies creating with `fast_model: null` and an initial `key` creates both the provider and its `"Default"` key in one call, and verifies `PATCH` can set and clear `fast_model`.
    - Add `discover_models_with_inline_key_stored_provider_key_and_422_on_bad_token` (Review Focus #5): mounts wiremock `GET /models`, tests `POST /api/v1/ai-providers/models` with an inline `key`, with `provider_id` (using stored key), and verifies HTTP 401 from upstream maps to 422 `token_invalid`.
    - Add `test_key_defaults_to_effective_fast_model_when_omitted`: verifies `POST /api/v1/ai-keys/{id}/test` with `{}` uses `fast_model` when set and falls back to `primary_model` when `fast_model` is `null`.
    - Update `settings_roundtrip_and_validation` for provider-only selection (`default_provider_id` without `default_model`).
  - In `crates/server/tests/connections.rs`:
    - Update repository `PATCH` test to verify `review_provider_id` (including 404 for unknown provider UUID and `null` to clear).
  - In `crates/server/tests/doctor.rs`:
    - Update `healthy_world`, replace `default_model` tests with `doctor_not_running_without_default_provider` and `doctor_not_running_when_default_provider_disabled` (Review Focus #5).
  - Update `crates/server/tests/settings_ignore.rs` and `crates/server/tests/usage.rs` JSON payloads where `"default_model": null` was sent.

- [ ] **Step 2: Run tests to verify they fail**
  - Run: `cargo test -j 2 -p adus-server -p adus`
  - Expected: FAIL

- [ ] **Step 3: Implement `api/ai.rs`, `api/settings.rs`, `api/repositories.rs`, and `doctor.rs`**
  - Implement all interfaces specified above.

- [ ] **Step 4: Run tests to verify they pass**
  - Run: `cargo test -j 2 -p adus-server -p adus`
  - Expected: PASS

- [ ] **Step 5: Commit**
  - `git add crates/server`
  - `git commit -m "feat(server): add model discovery endpoint and provider model tiers"`

---

### Task 5: Dashboard UI (`provider-dialog.svelte` Combobox, `ai-providers`, `settings`, `repositories`) & Full Workspace Verification

**Files:**
- Modify: `web/src/lib/components/provider-dialog.svelte`, `web/src/routes/ai-providers/+page.svelte`, `web/src/routes/settings/+page.svelte`, `web/src/routes/repositories/+page.svelte`

**Interfaces:**
- `web/src/lib/components/provider-dialog.svelte`:
  - `Provider` type adds `primary_model: string` and `fast_model: string | null`.
  - **Step 1 — Provider API & Connection Test:**
    - Fields: **Kind** (on create), **Name**, **Base URL** (shown when `shownKind === 'openai_compatible'`), **API key** (`type="password"`, required on create, optional on edit when `provider.keys.length > 0`), and a **Test connection** button (`POST /api/v1/ai-providers/models`).
    - Modifying `kind`, `baseUrl`, or `apiKey` resets `tested = false` and `models = []`.
    - When **Test connection** succeeds, sets `tested = true` and `models = reply.models`, and toasts `"Connected — {models.length} models loaded"`.
  - **Step 2 — Interactive Model Comboboxes (`Popover` + `Command` from `#lib/components/ui/popover/index.js` & `#lib/components/ui/command/index.js`):**
    - **Primary model (required)** and **Fast model (optional)** Comboboxes are **disabled while `!tested`**.
    - Once `tested === true`, each Combobox lets the user search/filter `models` interactively (and select a custom typed model name if their search query is not in `models`).
    - The **Fast model** Combobox includes a top option `"None (use Primary model)"` (`""`).
    - Submit button (**Add** / **Save**) is disabled while `busy || !tested || !primaryModel.trim()`.
    - Submitting create sends `{ kind, name, base_url: url || null, primary_model: primaryModel.trim(), fast_model: fastModel.trim() || null, key: apiKey.trim(), key_label: 'Default' }`.
    - Submitting edit sends `{ name, base_url: url, primary_model: primaryModel.trim(), fast_model: fastModel.trim() || null }` (and if the admin also entered a new `apiKey` while editing a provider with 0 keys, posts it to `/ai-providers/${provider.id}/keys`).
- `web/src/routes/ai-providers/+page.svelte`:
  - Displays `Primary: {p.primary_model} · Fast: {p.fast_model ?? 'uses primary'}` (plus `p.base_url` when present) in `Card.Description`.
  - Clicking **Test** on an API key runs a 1-click `POST /ai-keys/${k.id}/test` with `{}` (removing the old `testOpen` modal dialog).
- `web/src/routes/settings/+page.svelte`:
  - Removes **Default model** `<Input>`, keeping **Default provider** `<Select>`.
- `web/src/routes/repositories/+page.svelte`:
  - Loads `/ai-providers` alongside `/connections` and `/repositories`.
  - Replaces static `Model override` column with **Provider** `<Select>` (`Default (global)` or any provider; read-only text label for `user` role).

- [ ] **Step 1: Update `provider-dialog.svelte`, `ai-providers/+page.svelte`, `settings/+page.svelte`, and `repositories/+page.svelte`**
  - Follow `DESIGN.md` tokens and `#lib/*` imports strictly (never `$lib/*`).

- [ ] **Step 2: Format, typecheck, and build the web app**
  - Run: `pnpm -C web fmt && pnpm -C web check && pnpm -C web build`
  - Expected: `svelte-check found 0 errors and 0 warnings` and `web/build` generated for `rust-embed`.

- [ ] **Step 3: Run full workspace Rust fmt, clippy, and test suite**
  - Run:
    - `cargo fmt --all -- --check`
    - `cargo clippy --workspace --all-targets -- -D warnings`
    - `cargo test --workspace -j 2`
  - Expected: 0 warnings, all tests PASS.

- [ ] **Step 4: Commit**
  - `git add web`
  - `git commit -m "feat(web): add interactive model combobox, connection test, and provider selection UI"`
