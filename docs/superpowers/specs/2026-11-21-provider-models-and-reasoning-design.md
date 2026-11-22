# Provider Model Tiers (`primary_model` / Optional `fast_model`), Interactive Model Discovery & Reasoning Effort — Design Specification

Date: 2026-11-21 · Status: approved

> **Binding scope & policy:** This specification extends [2026-11-19-adus-design.md](2026-11-19-adus-design.md) and [2026-11-21-agentic-review-engine-design.md](2026-11-21-agentic-review-engine-design.md), and inherits Section 6.13 (AGPL-3.0 clean-room policy: zero code, prompts, or text derived from Kodus `libs/ee/**` or `*.ee.*`).

---

## 1. Intent & Why

Previously, Adus configured a single raw model string on `settings.default_model` (or `repositories.review_model`) and required typing a raw model string in a modal every time an admin tested an API key. That had three drawbacks:

1. **No separation between primary discovery and fast utility tasks:** Multi-stage review passes (Finder vs. Verifier/Dedup) and key health pings could not split workloads between a flagship reasoning model (`primary_model`) and a lower-latency/cheaper model (`fast_model`).
2. **Manual model typing without connection verification:** Admins had to hand-type exact model IDs in Settings, Repositories, and the key-test modal without testing the provider connection or browsing available models from the provider's API when creating a provider.
3. **No reasoning effort control:** `crates/llm` had no way to adjust reasoning/thinking effort (`Low | Medium | High`) per task.

This design moves model configuration onto each **AI Provider** as a required **`primary_model`** plus an optional **`fast_model`** (falling back to `primary_model` when unset), adds interactive model discovery (`list_models` + searchable Combobox unlocked after testing the provider API connection), drops `settings.default_model` and `repositories.review_model`, and adds an auto-adjusted **`ReasoningEffort`** handler in `crates/llm` with automatic fallback for non-thinking models.

---

## 2. Goals, Non-Goals & Anti-Goals

### Goals
1. **Per-provider model tiers (`primary_model` & optional `fast_model`):**
   - Every `ai_providers` row stores a required non-empty `primary_model: String` and an optional `fast_model: Option<String>`.
   - When `fast_model` is `None`, `AiProvider::effective_fast_model()` returns `&self.primary_model`.
2. **Interactive provider connection test & model Combobox discovery:**
   - `LlmClient::list_models(&self, api_key: &str)` fetches the live model catalog from OpenAI (`/models`), OpenAI-compatible (`/models`), Anthropic (`/v1/models`), and Gemini (`/v1beta/models`).
   - `POST /api/v1/ai-providers/models` tests the connection using an inline `key` (when creating/editing) or an existing stored key for `provider_id` (when editing) and returns `{ models: Vec<String> }`.
   - In the frontend `ProviderDialog`, the **Primary model** (required) and **Fast model** (optional) Combobox selectors (`Popover` + `Command`) remain **disabled until the user enters the provider API details/key and tests the connection**.
   - `POST /api/v1/ai-providers` accepts an optional initial `key` (and `key_label`, defaulting to `"Default"`) so the key tested during provider creation is saved immediately with the provider.
3. **Provider-only selection in Settings & Repositories:**
   - `settings` keeps `default_provider_id` and drops `default_model`.
   - `repositories` keeps `review_provider_id` (per-repo AI Provider override) and drops `review_model`.
4. **Auto-adjusted `ReasoningEffort` (`Low | Medium | High`) in `crates/llm`:**
   - **Finder (`Specialists` mode):** `provider.primary_model` + `ReasoningEffort::High`.
   - **Finder (`Generalist` mode):** `provider.primary_model` + `ReasoningEffort::Medium`.
   - **Verifier / Semantic Dedup (when invoked):** `provider.effective_fast_model()` + `ReasoningEffort::Low`.
   - **Test Key (`POST /api/v1/ai-keys/{id}/test`):** `provider.effective_fast_model()` (unless an explicit `model` is passed) + `effort: None`.
5. **Transparent fallback for non-reasoning models:** When a provider returns HTTP 400 or 422 rejecting reasoning/thinking parameters, `LlmClient::chat` automatically retries once with `effort: None`.

### Non-Goals
- User-facing UI knobs for reasoning token budgets or effort per task (effort is auto-adjusted by task and review mode).
- Duplicating the agentic pipeline stages (`specialists`, `verifier`, `dedup`) covered in [2026-11-21-agentic-review-engine.md](../plans/2026-11-21-agentic-review-engine.md).

### Anti-Goals
- Deriving any code, prompts, or text from Kodus `libs/ee/**` or `*.ee.*`.
- Failing a review or invalidating an API key when a non-reasoning model rejects `reasoning_effort` or `thinking`.

---

## 3. Detailed Component Specifications

### 3.1 Database Schema & Migration (`migrations/0008_review_mode_and_provider_models.sql`)

The provider model columns share migration 0008 with `review_mode` (agentic review engine spec §4.1): neither was released on its own, and they ship together.

- `ai_providers` gains `primary_model TEXT NULL` and `fast_model TEXT NULL`.
- **Backfill without inventing a model:** `primary_model` comes from `settings.default_model` when the provider was the default, else from the newest `repositories.review_model` of a repository that still exists (`deleted_at IS NULL`), both trimmed. A provider with neither stays `NULL`: it **needs setup**, and the engine, doctor and test-key treat it as having no model until an admin picks one. (A made-up placeholder such as `'default'` would pass every check and then fail every review.)
- **Provider-only repo overrides:** the old engine ignored a repo override that named a provider but no model and used the global provider and model. The migration keeps that by clearing `review_provider_id` on those rows.
- Then it drops `settings.default_model` and `repositories.review_model`.
- Pinned by `migration_8_backfills_provider_models_without_inventing_one` (`crates/db/src/ai.rs`), which runs migrations 1–7, seeds old-style rows, then runs 8.

#### `crates/db` Structs & Queries
- `adus_db::ai::AiProvider`:
  - Adds `pub primary_model: Option<String>` (`None` = needs setup) and `pub fast_model: Option<String>`.
  - Helpers (blank counts as unset):
    - `pub fn primary_model(&self) -> Option<&str>`
    - `pub fn fast_model(&self) -> Option<&str>`
    - `pub fn effective_fast_model(&self) -> Option<&str>`: `self.fast_model()`, else `self.primary_model()`.
- `adus_db::ai::insert_provider(pool, kind, name, base_url, primary_model, fast_model: Option<&str>) -> Result<AiProvider>`.
- `adus_db::ai::update_provider(pool, id, name, base_url, primary_model: Option<&str>, fast_model: Option<&str>, enabled) -> Result<Option<AiProvider>>` (a needs-setup provider can still be renamed or toggled).
- `adus_db::settings::Settings`: drops `default_model: Option<String>`.
- `adus_db::repositories::{Repository, RepoPatch}`: drops `review_model`.

---

### 3.2 `ReasoningEffort`, Model Discovery (`list_models`) & Provider Wire Mapping (`crates/llm`)

#### Types (`crates/llm/src/types.rs`)
```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChatRequest {
    pub model: String,
    pub system: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDef>,
    pub max_tokens: u32,
    pub effort: Option<ReasoningEffort>,
}
```
- `ChatRequest::text(model, system, user, max_tokens)` sets `effort: None`.
- `ChatRequest::with_effort(mut self, effort: ReasoningEffort) -> Self` sets `self.effort = Some(effort)`.
- `LlmClient` trait adds:
  ```rust
  #[async_trait]
  pub trait LlmClient: Send + Sync {
      async fn chat(&self, api_key: &str, req: &ChatRequest) -> Result<ChatResponse, LlmError>;
      async fn list_models(&self, api_key: &str) -> Result<Vec<String>, LlmError>;
  }
  ```

#### `list_models` Wire Specifics
1. **OpenAI & OpenAI-Compatible (`crates/llm/src/openai.rs`):**
   - `GET {self.base_url}/models` with `Authorization: Bearer {api_key}`.
   - Parses `{"data": [{"id": String}]}` (or top-level `[{"id": String}]`).
   - Trims non-empty IDs, sorts alphabetically, and deduplicates.
2. **Anthropic (`crates/llm/src/anthropic.rs`):**
   - `GET {self.base_url}/v1/models` with `x-api-key: {api_key}` and `anthropic-version: 2023-06-01`.
   - Parses `{"data": [{"id": String}]}`, trims non-empty IDs, sorts alphabetically, and deduplicates.
3. **Gemini (`crates/llm/src/gemini.rs`):**
   - `GET {self.base_url}/v1beta/models` with `x-goog-api-key: {api_key}`.
   - Parses `{"models": [{"name": String, "supportedGenerationMethods": Option<Vec<String>>}]}`.
   - Filters models where `supported_generation_methods` is `None` or contains `"generateContent"`, strips any leading `"models/"` prefix from `name`, trims non-empty IDs, sorts alphabetically, and deduplicates.

#### Reasoning Effort Wire Specifics & Transparent Retry
- `REASONING_UNSUPPORTED_MARKERS`:
  - `"reasoning_effort"`, `"reasoning effort"`, `"thinking"` (also matches `"thinking_config"`, `"thinkingconfig"`, `"thinking_budget"`, `"thinkingbudget"`), `"budget_tokens"`, `"does not support reasoning"`.
- When `req.effort.is_some()` and a call returns `LlmError::Invalid(msg)` whose lowercase message contains any `REASONING_UNSUPPORTED_MARKERS`, `LlmClient::chat` transparently retries **once** with `effort: None`.
- **Output headroom:** every provider counts reasoning tokens against the same output cap as the answer, so a request with an effort asks for `max_tokens + reserve`. `ReasoningEffort::output_reserve()` is `Low => 2_048`, `Medium => 4_096`, `High => 8_192` (OpenAI/compatible and Anthropic); Gemini adds its own thinking budget. Without this, `High` on Gemini (budget 8192 = the engine's 8192 cap) or on OpenAI reasoning models can spend the whole cap thinking and return no answer.
- **Timeout:** a request with an effort uses a 300 s per-request timeout instead of 120 s (`types::request_timeout`), since a long think plus the answer can run past two minutes. The retry without effort uses 120 s.
- **OpenAI & OpenAI-Compatible (`openai.rs`):**
  - When `effort` is `Some(e)`, sets `body["reasoning_effort"] = "low" | "medium" | "high"` and raises the token field (`max_completion_tokens` / `max_tokens`) by `e.output_reserve()`.
- **Anthropic (`anthropic.rs`):**
  - Budget: `e.output_reserve()` (`Low => 2_048`, `Medium => 4_096`, `High => 8_192`).
  - When `effort` is `Some(e)`, sets `body["thinking"] = json!({"type": "enabled", "budget_tokens": budget})` and `body["max_tokens"] = req.max_tokens.saturating_add(budget)`.
  - Preserves `Block::Thinking { thinking, signature }` and `Block::RedactedThinking { data }` in `tool_calls[0].provider_meta` (as a JSON array of raw block objects) and replays them at the start of `assistant_blocks()` on subsequent turns.
- **Gemini (`gemini.rs`):**
  - Budgets: `Low => 1_024`, `Medium => 4_096`, `High => 8_192`.
  - When `effort` is `Some(e)`, sets `body["generationConfig"]["thinkingConfig"] = json!({"thinkingBudget": budget})` and `maxOutputTokens = req.max_tokens + budget`.
  - Deserializes `#[serde(default)] thought: bool` on `Part` and skips `part.thought == true` when concatenating `ChatResponse.text`.

---

### 3.3 `AgentLlm` & `Engine` Model/Effort Resolution (`crates/review`)

- `AgentLlm<'a>` in `crates/review/src/agent.rs` adds `pub effort: Option<ReasoningEffort>` and passes `effort: llm.effort` into `ChatRequest` in `request()`.
- Helper `fn finder_effort(mode: ReviewMode) -> ReasoningEffort`:
  - `ReviewMode::Generalist` → `ReasoningEffort::Medium`
  - `ReviewMode::Specialists` → `ReasoningEffort::High`
- **Provider resolution in `Engine::run` (`crates/review/src/engine.rs`):**
  1. `let provider_id = repo.review_provider_id.or(settings.default_provider_id);`
  2. Resolve `provider` via `ai::get_provider(&self.db, id).await?`, requiring `provider.enabled` and `provider.primary_model()` to be `Some`.
  3. If missing, disabled, or the provider has no primary model (needs setup): handle with existing `no_model` behavior (`Outcome::Skipped("no_model")` on `Trigger::Updated`; PR comment + `EngineError::Fatal("no review model configured")` on `Trigger::Opened | Trigger::Command`). The comment says to pick a provider under Settings and its model under AI Providers.
  4. Resolve `mode = mode_override.or(repo.review_mode).unwrap_or(settings.review_mode)` and pass the primary model with `finder_effort(mode)` to review calls.
  5. The agentic engine's Verifier and Dedup use `effective_fast_model()` with `ReasoningEffort::Low` (agentic review engine spec, decision 10).

---

### 3.4 Server REST APIs & `adus doctor` (`crates/server`)

#### `/api/v1/ai-providers`, `/api/v1/ai-providers/models` & `/api/v1/ai-keys/{id}/test` (`crates/server/src/api/ai.rs`)
- **`POST /api/v1/ai-providers/models` (`discover_models`):**
  - Admin-only (`AdminUser`).
  - Body `DiscoverModels { kind: Option<ProviderKind>, base_url: Option<String>, key: Option<String>, provider_id: Option<Uuid>, #[serde(default)] refresh: bool }`.
  - Resolves `kind` and `base_url` (from `provider_id` if given, else from `body.kind` and `body.base_url`), resolves `secret` (inline `body.key` if non-empty, else first usable/enabled key of `provider_id`), calls `client_for(...).list_models(&secret).await`, maps `LlmError::Unauthorized` → 422 `token_invalid`, other errors → 502 `provider_error`, and returns `200 OK` `{"models": Vec<String>, "cached": bool}`.
  - **A stored key only goes to the saved base URL.** With `provider_id`, a `base_url` that differs from the saved one (ignoring a trailing `/`) requires an inline `key`; otherwise 422 `validation_error` and nothing is sent. The dialog asks for the key again when the URL changes.
  - **Model-list cache:** successful non-empty lists are cached in Redis for 1 hour under `{prefix}:cache:models:v2:<hex>`, where `<hex>` is an HMAC-SHA256 under `ADUS_SECRET_KEY` of kind, normalized base URL and API key (so no key name is derived from the API key alone). `refresh: true` skips the read; a 401 evicts the entry. The dashboard's **Test connection** always sends `refresh: true`, because a connection test must ask the provider (a revoked key must not show "Connected").
- **`POST /api/v1/ai-providers` (`CreateProvider`):**
  - Body: `{ kind: ProviderKind, name: String, base_url: Option<String>, primary_model: String, fast_model: Option<String>, key: Option<String>, key_label: Option<String> }`.
  - Validates `name` and `primary_model` via `required(...)`; trims `fast_model` (empty string becomes `None`).
  - When `body.key` is `Some(raw)` and non-empty after trim, inserts the initial `AiKey` (with `label = key_label` defaulting to `"Default"`) and includes it in `ProviderJson.keys`.
- **`PATCH /api/v1/ai-providers/{id}` (`UpdateProvider`):**
  - Body: `{ name: Option<String>, #[serde(default, deserialize_with = "present_or_null")] base_url: Option<Option<String>>, primary_model: Option<String>, #[serde(default, deserialize_with = "present_or_null")] fast_model: Option<Option<String>>, enabled: Option<bool> }`.
- **`ProviderJson`:** includes `primary_model: Option<String>` (`null` = needs setup) and `fast_model: Option<String>`. A PATCH without `primary_model` keeps the current value, so a needs-setup provider can still be renamed or toggled.
- **`POST /api/v1/ai-keys/{id}/test` (`TestKey`):**
  - Body `TestKey { #[serde(default)] model: Option<String> }`; when `model` is `None` or blank, defaults to `provider.effective_fast_model()`; a provider with no model at all → 422 `validation_error` ("choose a primary model first"), no provider call.
  - The ping asks for up to 512 output tokens (`TEST_KEY_MAX_TOKENS`). A tiny cap made OpenRouter-routed and reasoning models fail the ping by running out of tokens before answering, which looked like a broken key.

#### `/api/v1/settings` & `/api/v1/repositories`
- `settings.rs`: drops `default_model` from `SettingsJson` and `SettingsBody`; when `default_provider_id` is `Some(id)`, verifies `ai::get_provider` exists (else 404 `not_found`).
- `repositories.rs`: drops `review_model` from `RepositoryJson` and `UpdateRepository`; when `review_provider_id` is `Some(Some(id))`, verifies `ai::get_provider` exists (else 404 `not_found`).

#### `adus doctor` (`crates/server/src/doctor.rs`)
- `check_settings`:
  - Verifies `settings.default_provider_id` is `Some(id)` and `ai::get_provider` returns an enabled provider with non-empty `primary_model`.
  - On success:
    - If `provider.fast_model` is `Some(fast)` (non-empty): `Check::ok(format!("Default provider: {} ({} / {})", provider.name, provider.primary_model, fast))`.
    - Else: `Check::ok(format!("Default provider: {} ({})", provider.name, provider.primary_model))`.
  - On failure: `Check::problem(Status::Fail, "Default provider", ...)`.

---

### 3.5 Dashboard UI (`web/`)

1. **Provider Dialog (`web/src/lib/components/provider-dialog.svelte`):**
   - `Provider` type adds `primary_model: string | null` and `fast_model: string | null`.
   - **Step 1 — Provider API & Connection Test:**
     - Fields: **Kind** (on create), **Name**, **Base URL** (when `shownKind === 'openai_compatible'`), **API key** (`type="password"`, required on create, optional on edit when `provider.keys.length > 0`), and a **Test connection** button (`POST /api/v1/ai-providers/models`).
     - Editing `kind`, `baseUrl`, or `apiKey` resets `tested = false` and `models = []`.
     - **Test connection** sends `refresh: true`; on success sets `tested = true` and `models = reply.models`.
     - When editing, a Base URL different from the saved one makes the API key required again (a saved key is never sent to a new URL). A key typed while editing is saved as another key of the provider (`Default` when it is the first).
   - **Step 2 — Interactive Model Comboboxes (`Popover` + `Command`):**
     - **Primary model (required)** and **Fast model (optional)** Comboboxes are **disabled while `!tested`**.
     - Once `tested === true`, each Combobox lets the user search/filter `models` interactively (and select a custom typed model name if their search query is not in `models`).
     - The **Fast model** Combobox includes a top option `"None (use Primary model)"` (`""`).
     - Submit button (**Add** / **Save**) is disabled while `busy || !tested || !primaryModel.trim()`.
2. **AI Providers Page (`web/src/routes/ai-providers/+page.svelte`):**
   - Shows `Primary: {p.primary_model} · Fast: {p.fast_model ?? 'uses primary'}` in `Card.Description`; a provider with no primary model shows "not set — edit to choose a model" in the destructive text color.
   - Clicking **Test** on an API key runs a 1-click `POST /api/v1/ai-keys/${k.id}/test` with `{}` (removing the old model-input modal).
3. **Settings Page (`web/src/routes/settings/+page.svelte`):**
   - Removes the **Default model** `<Input>`, keeping **Default provider** `<Select>`.
4. **Repositories Page (`web/src/routes/repositories/+page.svelte`):**
   - Replaces the static `Model override` column with an interactive **Provider** `<Select>` (`Default (global)` or any configured provider; muted text label for `user` role).
