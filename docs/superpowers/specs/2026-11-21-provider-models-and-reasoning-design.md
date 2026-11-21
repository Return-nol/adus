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

### 3.1 Database Schema & Migration (`migrations/0009_provider_models.sql`)

```sql
-- Each AI provider carries its own primary_model (required) and fast_model (optional;
-- NULL falls back to primary_model).
-- Backfill existing providers from settings.default_model or repositories.review_model before
-- dropping those columns.
ALTER TABLE ai_providers
    ADD COLUMN primary_model TEXT NULL,
    ADD COLUMN fast_model TEXT NULL;

UPDATE ai_providers p
SET primary_model = COALESCE(
    (SELECT s.default_model FROM settings s WHERE s.default_provider_id = p.id AND btrim(COALESCE(s.default_model, '')) <> '' LIMIT 1),
    (SELECT r.review_model FROM repositories r WHERE r.review_provider_id = p.id AND btrim(COALESCE(r.review_model, '')) <> '' ORDER BY r.created_at DESC LIMIT 1),
    'default'
);

ALTER TABLE ai_providers
    ALTER COLUMN primary_model SET NOT NULL;

ALTER TABLE settings
    DROP COLUMN default_model;

ALTER TABLE repositories
    DROP COLUMN review_model;
```

#### `crates/db` Structs & Queries
- `adus_db::ai::AiProvider`:
  - Adds `pub primary_model: String` and `pub fast_model: Option<String>`.
  - Adds helper:
    ```rust
    impl AiProvider {
        pub fn effective_fast_model(&self) -> &str {
            self.fast_model
                .as_deref()
                .map(str::trim)
                .filter(|m| !m.is_empty())
                .unwrap_or(&self.primary_model)
        }
    }
    ```
- `adus_db::ai::insert_provider(pool, kind, name, base_url, primary_model, fast_model: Option<&str>) -> Result<AiProvider>`.
- `adus_db::ai::update_provider(pool, id, name, base_url, primary_model, fast_model: Option<&str>, enabled) -> Result<Option<AiProvider>>`.
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
  - `"reasoning_effort"`, `"reasoning effort"`, `"thinking"`, `"thinking_config"`, `"thinkingconfig"`, `"thinking_budget"`, `"thinkingbudget"`, `"budget_tokens"`, `"does not support reasoning"`.
- When `req.effort.is_some()` and a call returns `LlmError::Invalid(msg)` whose lowercase message contains any `REASONING_UNSUPPORTED_MARKERS`, `LlmClient::chat` transparently retries **once** with `effort: None`.
- **OpenAI & OpenAI-Compatible (`openai.rs`):**
  - When `effort` is `Some(e)`, sets `body["reasoning_effort"] = "low" | "medium" | "high"`.
- **Anthropic (`anthropic.rs`):**
  - Budgets: `Low => 2_048`, `Medium => 4_096`, `High => 8_192`.
  - When `effort` is `Some(e)`, sets `body["thinking"] = json!({"type": "enabled", "budget_tokens": budget})` and `body["max_tokens"] = req.max_tokens.saturating_add(budget)`.
  - Preserves `Block::Thinking { thinking, signature }` and `Block::RedactedThinking { data }` in `tool_calls[0].provider_meta` (as a JSON array of raw block objects) and replays them at the start of `assistant_blocks()` on subsequent turns.
- **Gemini (`gemini.rs`):**
  - Budgets: `Low => 1_024`, `Medium => 4_096`, `High => 8_192`.
  - When `effort` is `Some(e)`, sets `body["generationConfig"]["thinkingConfig"] = json!({"thinkingBudget": budget})`.
  - Deserializes `#[serde(default)] thought: bool` on `Part` and skips `part.thought == true` when concatenating `ChatResponse.text`.

---

### 3.3 `AgentLlm` & `Engine` Model/Effort Resolution (`crates/review`)

- `AgentLlm<'a>` in `crates/review/src/agent.rs` adds `pub effort: Option<ReasoningEffort>` and passes `effort: llm.effort` into `ChatRequest` in `request()`.
- Helper `pub fn finder_effort(mode: ReviewMode) -> ReasoningEffort`:
  - `ReviewMode::Generalist` → `ReasoningEffort::Medium`
  - `ReviewMode::Specialists` → `ReasoningEffort::High`
- **Provider resolution in `Engine::run` (`crates/review/src/engine.rs`):**
  1. `let provider_id = repo.review_provider_id.or(settings.default_provider_id);`
  2. Resolve `provider` via `ai::get_provider(&self.db, id).await?`, requiring `provider.enabled && !provider.primary_model.trim().is_empty()`.
  3. If missing, disabled, or `primary_model` is blank: handle with existing `no_model` behavior (`Outcome::Skipped("no_model")` on `Trigger::Updated`; PR comment + `EngineError::Fatal("no review model configured")` on `Trigger::Opened | Trigger::Command`).
  4. Resolve `mode = mode_override.or(repo.review_mode).unwrap_or(settings.review_mode)` and pass `provider.primary_model` with `finder_effort(mode)` to review calls.

---

### 3.4 Server REST APIs & `adus doctor` (`crates/server`)

#### `/api/v1/ai-providers`, `/api/v1/ai-providers/models` & `/api/v1/ai-keys/{id}/test` (`crates/server/src/api/ai.rs`)
- **`POST /api/v1/ai-providers/models` (`discover_models`):**
  - Admin-only (`AdminUser`).
  - Body `DiscoverModels { kind: Option<ProviderKind>, base_url: Option<String>, key: Option<String>, provider_id: Option<Uuid> }`.
  - Resolves `kind` and `base_url` (from `provider_id` if given, else from `body.kind` and `body.base_url`), resolves `secret` (inline `body.key` if non-empty, else first usable/enabled key of `provider_id`), calls `client_for(...).list_models(&secret).await`, maps `LlmError::Unauthorized` → 422 `token_invalid`, other errors → 502 `provider_error`, and returns `200 OK` `{"models": Vec<String>}`.
- **`POST /api/v1/ai-providers` (`CreateProvider`):**
  - Body: `{ kind: ProviderKind, name: String, base_url: Option<String>, primary_model: String, fast_model: Option<String>, key: Option<String>, key_label: Option<String> }`.
  - Validates `name` and `primary_model` via `required(...)`; trims `fast_model` (empty string becomes `None`).
  - When `body.key` is `Some(raw)` and non-empty after trim, inserts the initial `AiKey` (with `label = key_label` defaulting to `"Default"`) and includes it in `ProviderJson.keys`.
- **`PATCH /api/v1/ai-providers/{id}` (`UpdateProvider`):**
  - Body: `{ name: Option<String>, #[serde(default, deserialize_with = "present_or_null")] base_url: Option<Option<String>>, primary_model: Option<String>, #[serde(default, deserialize_with = "present_or_null")] fast_model: Option<Option<String>>, enabled: Option<bool> }`.
- **`ProviderJson`:** includes `primary_model: String` and `fast_model: Option<String>`.
- **`POST /api/v1/ai-keys/{id}/test` (`TestKey`):**
  - Body `TestKey { #[serde(default)] model: Option<String> }`; when `model` is `None` or blank, defaults to `provider.effective_fast_model().to_string()`.

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
   - `Provider` type adds `primary_model: string` and `fast_model: string | null`.
   - **Step 1 — Provider API & Connection Test:**
     - Fields: **Kind** (on create), **Name**, **Base URL** (when `shownKind === 'openai_compatible'`), **API key** (`type="password"`, required on create, optional on edit when `provider.keys.length > 0`), and a **Test connection** button (`POST /api/v1/ai-providers/models`).
     - Editing `kind`, `baseUrl`, or `apiKey` resets `tested = false` and `models = []`.
     - When **Test connection** succeeds, sets `tested = true` and `models = reply.models`.
   - **Step 2 — Interactive Model Comboboxes (`Popover` + `Command`):**
     - **Primary model (required)** and **Fast model (optional)** Comboboxes are **disabled while `!tested`**.
     - Once `tested === true`, each Combobox lets the user search/filter `models` interactively (and select a custom typed model name if their search query is not in `models`).
     - The **Fast model** Combobox includes a top option `"None (use Primary model)"` (`""`).
     - Submit button (**Add** / **Save**) is disabled while `busy || !tested || !primaryModel.trim()`.
2. **AI Providers Page (`web/src/routes/ai-providers/+page.svelte`):**
   - Shows `Primary: {p.primary_model} · Fast: {p.fast_model ?? 'uses primary'}` in `Card.Description`.
   - Clicking **Test** on an API key runs a 1-click `POST /api/v1/ai-keys/${k.id}/test` with `{}` (removing the old model-input modal).
3. **Settings Page (`web/src/routes/settings/+page.svelte`):**
   - Removes the **Default model** `<Input>`, keeping **Default provider** `<Select>`.
4. **Repositories Page (`web/src/routes/repositories/+page.svelte`):**
   - Replaces the static `Model override` column with an interactive **Provider** `<Select>` (`Default (global)` or any configured provider; muted text label for `user` role).
