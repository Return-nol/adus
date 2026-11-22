# Adus Agentic Review Engine — Design Specification

Date: 2026-11-21 · Status: approved via `/grill-me`, ready for implementation

> **Binding scope & policy:** This specification extends [2026-11-19-adus-design.md](2026-11-19-adus-design.md) and inherits its AGPL-3.0 clean-room policy (Section 6.13: never derive code, prompts, or text from `libs/ee/**` or `*.ee.*` in Kodus; all prompts, schemas, and tool engines are authored natively for Adus).

---

## 1. Intent & Goals

The current review engine reviews each changed file in isolation in a single LLM pass (`build_prompt` → raw JSON array → `keep_valid` line/path check). Because the model only sees one file's patch and 3-line hunk context, it cannot trace callers across files, verify upstream guards, inspect type definitions, or double-check suspicious claims before posting.

This wave brings **Point 1 (Cross-file PR & codebase awareness)**, **Point 2 (Two-pass Finder → Verifier + Semantic Deduplication)**, and **auto-loading repository instruction files** to Adus while preserving Adus's lightweight **3-container footprint** (`adus` + Postgres + Redis) and **BYOK multi-key router**.

### Goals

1. **Full codebase context via ephemeral hardened shallow clone + read-only ReAct tools** (`read_file`, `grep`, `list_dir`, `find_files`) across all four LLM providers (`Openai`, `Anthropic`, `Gemini`, `OpenaiCompatible`).
2. **Two review modes:**
   - **`generalist`** (default): one PR-wide ReAct Finder per diff batch (with matching team rules included directly), balancing review depth and token cost.
   - **`specialists`**: parallel `Bug`, `Security`, and `Performance` specialists, plus a conditional `Rules` specialist (spawned only when matching enabled team rules exist), sharing a single read-only clone and per-review tool-call cache.
   - Configurable globally in Settings, overridable per repository, and invocable per PR via `@adudu review deep` (or `@adudu review specialists`).
3. **False-positive suppression (Point 2):**
   - **Per-finding tool-enabled Verifier** (concurrent up to 4, max 6 steps, capped at 25 candidates prioritized by severity) with **refute-to-drop, keep-by-default** semantics and **fail-closed** safety.
   - **PR-wide Semantic Deduplication** (single-turn pass when ≥ 2 verified findings remain) to merge overlapping findings (especially across specialists), with safe fallback.
4. **Prompt caching & context control:** Prefix-cache-friendly prompt ordering, Anthropic `cache_control: ephemeral` breakpoints + accurate cache token accounting, fixed token/char budgets, and older tool-result compaction.
5. **Resilient degradation:** Automatic fallback to self-contained diff-only review (with PR manifest, diff batching, diff-only Verifier, and summary warning) when cloning fails/times out (non-auth) or a model rejects tool calling (`ToolsUnsupported`).
6. **Repository instruction auto-loading:** Read `AGENTS.md`, `CLAUDE.md`, `.github/copilot-instructions.md`, and `.cursor/rules/*.mdc` from the cloned repo (bounded, wrapped as untrusted data).
7. **Idiomatic Rust engineering:** Follow the standards in Section 5 (based on Apollo GraphQL's Rust Best Practices) — type-driven safety (`RepoPath`, RAII `RepoWorkspace`, closed enums, typed serde tool inputs), clean borrowing (`&str`, `&[T]`, `Cow<'_, str>`), explicit `thiserror` error hierarchies, zero `.unwrap()`/`.expect()` outside tests, static dispatch internally, and `tokio::task::spawn_blocking` for filesystem/regex work.

### Non-Goals (Deferred)

- **Point 3 (Previous-review findings table & repeat suppression):** Deferred to the next wave (`review_findings` persistence + `VERIFIER_REPEAT_RULE`).
- **Executing repo code or compilers in worker:** `tsc`, `cargo check`, linters, arbitrary shell execution, and separate sandbox containers (e2b/Docker-in-Docker) remain strictly out of scope to keep the 3-container deployment and protect `ADUS_SECRET_KEY`, `DATABASE_URL`, `REDIS_URL`, and decrypted PATs.
- **Stage 3 post-verify Enrichment/Calibration LLM pass:** Kept as a 2-stage funnel (`Finder` emits full `Suggestion` → `Verifier` → `Dedup`).
- **Per-stage model settings in the UI:** models come from the review's AI provider (`primary_model`, optional `fast_model`, see [2026-11-21-provider-models-and-reasoning-design.md](2026-11-21-provider-models-and-reasoning-design.md)); stages pick a tier and a reasoning effort automatically (decision 10).
- **LLM-generated PR walkthroughs / auto-approval / custom agents / AST graph DB:** Out of scope for this wave.

---

## 2. Settled Design Decisions

| # | Area | Decision |
|---|---|---|
| 1 | **Clone consumption** | Multi-turn tool-calling ReAct loop across all 4 `crates/llm` providers (`read_file`, `grep`, `list_dir`, `find_files`, `submit_findings` / `submit_verdict`) |
| 2 | **Security boundary** | Strictly read-only pure-Rust tools inside the `adus` worker (`git` added to `Dockerfile`; `ignore` + `regex` in Rust, zero shell/compiler execution). Hardened `git` flags, PAT passed as `http.extraHeader` through `GIT_CONFIG_*` env vars (never argv, URL or `.git/config`), canonical `RepoPath` containment, `.git/**` & secret-file denylist |
| 3 | **Clone lifecycle** | Ephemeral shallow fetch per review in `tempfile::TempDir` under `<temp_dir>/adus-clones/`, deleted automatically on `Drop` (RAII) + startup sweep on `adus serve`; no extra Docker volume |
| 4 | **Fallback behavior** | Graceful fallback to self-contained diff-only review + summary warning line (`Codebase context unavailable — reviewed diff only`) if clone times out/fails (non-auth) or model returns `ToolsUnsupported`. Auth failures (401/403) still mark connection `invalid` and fail `Fatal` |
| 5 | **Finder modes** | `ReviewMode::Generalist` (default) and `ReviewMode::Specialists` |
| 6 | **Mode configuration** | `settings.review_mode` global default + nullable `repositories.review_mode` override + `@adudu review deep` (`specialists` alias). `Specialists` runs `Bug`, `Security`, `Performance` + conditional `Rules` (only if matching enabled rules exist) |
| 7 | **Verifier & Dedup** | Per-finding tool-enabled Verifier (concurrency 4, max 6 steps, max 25 candidates sorted `Critical → Warning → Info`, refute-to-drop / keep-by-default) → 1 single-turn PR-wide Semantic Dedup call when `verified.len() >= 2` |
| 8 | **Partial failure policy** | Specialists: partial results allowed with summary warning; retry only if all specialists fail retryably. Generalist (one agent per batch, so a failure leaves files unreviewed): when some Finder task failed and every failure was retryable (`AllKeysFailed`, `NoKeys`, `Internal`; a deadline cut never is), the review is retried (`Retryable`, waiting for the first `AllKeysFailed`'s `retry_after`) instead of posting with those files skipped. Verifier: **fail-closed** (drop unverified finding + summary warning). Dedup: safe fallback (keep verified findings as-is) |
| 9 | **Output funnel** | 2-stage: Finder outputs full `Suggestion` (`file`, optional `start_line`, `line`, `severity`, `title`, `body`, optional `suggestion`; `start_line..=line` is the span of existing new-file lines a `suggestion` replaces, and `start_line` is set only when that span has more than one line — see §4.12), Verifier keeps/drops without rewriting, Dedup merges |
| 10 | **Model & prompt caching** | One provider per review (`repo.review_provider_id` → `settings.default_provider_id`). Finder uses its `primary_model` with effort by mode (`Generalist` → `Medium`, `Specialists` → `High`, `finder_effort`); Verifier and Dedup use `effective_fast_model()` (the `fast_model`, else `primary_model`) with `Low` effort. Shared static prompt sections placed before specialist `<Role>` for implicit prefix caching; Anthropic `cache_control: ephemeral` breakpoints + cache token accounting |
| 11 | **Context & budgets** | Batch diffs up to ~40K chars (full PR manifest in every batch); tool outputs capped (`read_file` ≤ 200 lines / 8K chars, `grep` ≤ 40 match groups / 8K chars, `list_dir`/`find_files` ≤ 150 entries / 4K chars); compact older tool results above ~160K chars; one-shot overflow recovery |
| 12 | **Time budget** | A review's budget follows its effective mode: 12 min (`REVIEW_TIMEOUT`) for Generalist, small PRs included, 30 min (`SPECIALISTS_REVIEW_TIMEOUT`) for Specialists. Stage deadlines are fractions of that budget: Finder (incl. clone) by 60 %, Verifier by 90 %, Dedup + post in the rest. Agents wrap up (submit) before their stage deadline when it gets close (Section 4.6 step 6); work still cut by a deadline is handled like a failure of that stage (Section 4.11), so a large PR still posts a partial review instead of timing out with nothing |
| 13 | **Wave scope** | Point 1 + Point 2 + auto-loading repo instruction files (`AGENTS.md`, `CLAUDE.md`, `.github/copilot-instructions.md`, `.cursor/rules/*.mdc`) from the clone. Point 3 deferred |
| 14 | **Small PR** | A small, low-risk PR (`settings.small_pr_max_lines`, default 100, `0` = off; at most 3 reviewed files; no risky path, no covering enabled `critical` rule) skips the clone and the tools and is reviewed diff-only in one Generalist pass; Verifier and Dedup still run. `@adudu review` / `review deep` always get the full review (Section 4.13) |
| 15 | **Token ceiling** | `settings.max_review_input_tokens` (default 2,000,000, range `0..=100,000,000`, `0` = no limit) caps the input tokens (cached included) one review run sends. One shared `TokenBudget` per run counts every successful Finder, Verifier and Dedup response. Once reached: running agents wrap up (Section 4.6 step 6), batches not yet started are skipped (`token_budget`, never retryable), Verifier candidates not yet started are verified **diff-only** (one fast-model call, no tools; still fail-closed, and their tokens still count) instead of with the repository tools, and Dedup still runs. The summary gets "Token budget reached ({used} of {limit} input tokens): {details}." only when the ceiling changed something, where details lists only what happened ("N batches not reviewed", "some reviewers stopped early" for Finder agents that wrapped up for `token_budget`, "N findings verified from the diff only"); a limit crossed only by calls that cut nothing adds no note. It is a soft cap: the overshoot is the calls already in flight plus at most one wrap-up call per running agent (≤ 8 Finder agents or ≤ 4 Verifier agents, each with its full context), plus the diff-only verification of remaining candidates. Migration 0008 also lowers the PR-size defaults to `max_files_per_review` 30 and `max_patch_lines_per_file` 1500 (Section 4.10) |

---

## 3. Architecture & Data Flow

### 3.1 Review Pipeline Overview

```
Webhook / @adudu review [deep]
        │
        ▼
  Engine::review_pr (12m REVIEW_TIMEOUT / 30m SPECIALISTS_REVIEW_TIMEOUT < 31m JOB_TIMEOUT < 35m LOCK_TTL)
        │
        ├─ 1. Check repo enabled, auto_review, draft, ignored_title_keywords, head_sha dedup
        ├─ 2. Resolve provider (repo override -> settings default; must be enabled with a
        │     primary_model, else no_model) & review_mode
        │     (command override -> repo override -> settings default)
        ├─ 3. Fetch PR diff (full or incremental) + apply ignore_paths, max_patch_lines_per_file, max_files_per_review
        ├─ 4. Build PR File Manifest + group diffs into ~40K-char directory-clustered batches
        ├─ 4b. Small PR (not for `@adudu review`; Section 4.13): skip step 5, no tools, mode forced to Generalist
        ├─ 5. Ephemeral Clone (`RepoCloner::clone_pr`, 60 s timeout)
        │     ├─ Auth error -> Connection Invalid + Fatal
        │     ├─ Non-auth error / timeout -> fallback to diff-only mode + Summary Warning
        │     └─ Ok -> RepoWorkspace (RAII TempDir) + load RepoInstructions + build Arc<Toolbox> (with shared ToolCache)
        ├─ 6. Finder Pass (must finish by the Finder deadline, 60 % of the run's budget)
        │     ├─ Batches run up to BATCH_CONCURRENCY = 2 at a time
        │     ├─ Generalist: 1 agent per batch (max 20 steps)
        │     ├─ Specialists: Bug, Security, Performance (+ Rules if matching rules exist) in parallel within a batch
        │     ├─ If `ToolsUnsupported` -> retry Finder once in diff-only self-contained mode + Summary Warning
        │     └─ Partial specialist failures -> keep surviving findings + Summary Warning; Generalist: when every failed task failed retryably -> Retryable (no post)
        ├─ 7. Deterministic Filter (`keep_valid_batch`: path in batch, `line` in commentable `+` lines, `start_line` kept only when it is in the same diff hunk as `line`) + Exact Dedup
        ├─ 8. Verifier Pass (when candidates > 0; must finish by the Verifier deadline, 90 % of the run's budget)
        │     ├─ Prioritize Critical -> Warning -> Info; cap at 25 (warn if truncated)
        │     ├─ Tool-enabled mode: 1 Verifier loop per candidate (concurrency 4, max 6 steps, shared ToolCache)
        │     ├─ Diff-only fallback mode: 1 single-turn Verifier per candidate against diff + rules + memories
        │     └─ Fail-closed: Verifier error or unparseable verdict drops the candidate + adds warning count
        ├─ 9. Semantic Dedup Pass (when verified >= 2; cut at 97 % of the run's budget)
        │     └─ 1 single-turn LLM call to merge overlapping root causes; safe fallback on error
        └─ 10. Post Review (`format_inline`, `format_summary` with counts + warnings + ignored + skipped) & record `reviews`
```

### 3.2 Crate Dependency Boundaries

Crate dependencies remain strictly acyclic:
- `adus-core`: `ReviewMode`, `Job::ReviewPr.mode_override` (`#[serde(default)]`).
- `adus-db`: `settings.review_mode`, `repositories.review_mode` (migration `0008_review_mode_and_provider_models.sql`, which also carries the provider model tiers).
- `adus-git`: `CloneSpec` + `GitProvider::clone_spec(&self, repo, pr) -> Option<CloneSpec>` (optional method on trait so test fakes or mock providers can return `None`).
- `adus-llm`: Multi-turn `Message`, `MessageRole`, `ContentPart`, `ToolDef`, `ToolCall`, `StopReason`, `CacheStats`, `LlmError::{ToolsUnsupported, ContextOverflow}`, `ChatError::{ToolsUnsupported, ContextOverflow}`, Anthropic `cache_control` breakpoints, OpenAI/OpenAI-compatible `tools`, Gemini `functionDeclarations` + `thoughtSignature` preservation.
- `adus-review`: `workspace`, `tools`, `repo_instructions`, `agent`, `specialists`, `verifier`, `dedup`, updated `prompt`, `parse`, `command`, `engine`.
- `adus-server`: `settings` & `repositories` REST API exposure + `doctor` `git --version` check.
- `adus` (bin): `cleanup_orphaned_workspaces()` at startup in `serve`, wiring `GitCliCloner` into `Engine`.

---

## 4. Detailed Component Specifications

### 4.1 Review Modes, Database & Commands

#### `ReviewMode` (`crates/core/src/types.rs`)
```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "review_mode", rename_all = "snake_case")]
pub enum ReviewMode {
    #[default]
    Generalist,
    Specialists,
}
```

#### Migration `migrations/0008_review_mode_and_provider_models.sql`
The review-mode part is below; the same file also moves models onto AI providers (provider-models spec §3.1).
```sql
CREATE TYPE review_mode AS ENUM ('generalist', 'specialists');

ALTER TABLE settings
    ADD COLUMN review_mode review_mode NOT NULL DEFAULT 'generalist';

ALTER TABLE settings
    ADD COLUMN small_pr_max_lines INTEGER NOT NULL DEFAULT 100
        CHECK (small_pr_max_lines BETWEEN 0 AND 10000);

ALTER TABLE repositories
    ADD COLUMN review_mode review_mode NULL;
```

The same migration carries the cost controls (decision 15). The `settings` row already exists, so each new default is also written to the row while it still holds the old default (an admin's own value is kept): `max_files_per_review` 50 → 30, `max_patch_lines_per_file` 2000 → 1500. It adds `settings.max_review_input_tokens INTEGER NOT NULL DEFAULT 2000000 CHECK (0..=100000000)` and `llm_usage.cache_read_tokens` / `cache_write_tokens INTEGER NOT NULL DEFAULT 0` (`input_tokens` keeps meaning total input, cached included).

#### Queue Backwards Compatibility (`crates/core/src/job.rs`)
`Job::ReviewPr` adds:
```rust
#[serde(default)]
mode_override: Option<ReviewMode>,
```
Existing in-flight Redis stream entries without `mode_override` deserialize cleanly as `None`.

#### PR Commands (`crates/review/src/command.rs`)
- `@adudu review` → `Command::Review { mode: None }`
- `@adudu review deep` or `@adudu review specialists` (case-insensitive, word boundary) → `Command::Review { mode: Some(ReviewMode::Specialists) }`
- `@adudu review generalist` → `Command::Review { mode: Some(ReviewMode::Generalist) }`
- Any other text after `@adudu review` (e.g. `@adudu review please`) still triggers `Command::Review { mode: None }` so existing UX is preserved unless the second word is explicitly `deep`, `specialists`, or `generalist`.
- Update `HELP` text in `command.rs` to document `@adudu review deep`.

#### Mode Resolution Precedence
1. `Job::ReviewPr { mode_override: Some(m), .. }` (from `@adudu review deep` / `generalist`)
2. `repo.review_mode` (when `Some(m)`)
3. `settings.review_mode` (global default, `Generalist`)

---

### 4.2 Multi-Turn LLM & Prompt Caching (`crates/llm`)

#### Request & Response Types (`crates/llm/src/types.rs`)
```rust
pub struct ChatRequest {
    pub model: String,
    pub system: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDef>,
    pub max_tokens: u32,
    /// Reasoning effort (provider-models spec §3.2); `None` = provider default.
    pub effort: Option<ReasoningEffort>,
}
```
Provide `ChatRequest::text(model, system, user, max_tokens)` (effort `None`, set with `.with_effort(e)`) for ergonomic single-turn calls (used by `ai::test_key`, diff-only Verifier, Semantic Dedup, and existing tests).

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum Message {
    User { parts: Vec<UserPart> },
    Assistant { parts: Vec<AssistantPart> },
    Tool { results: Vec<ToolResult> },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum UserPart {
    Text(String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum AssistantPart {
    Text(String),
    ToolCall(ToolCall),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
    /// Opaque provider metadata (e.g., Gemini 2.5/3 `thoughtSignature`) preserved verbatim on turn echo.
    pub provider_meta: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    pub call_id: String,
    pub tool_name: String,
    pub content: String,
    pub is_error: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    Other,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChatResponse {
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
    pub stop_reason: StopReason,
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub cache_read_tokens: u32,
    pub cache_write_tokens: u32,
}
```

#### Error Classification (`LlmError` & `ChatError`)
- `LlmError::ToolsUnsupported(String)` / `ChatError::ToolsUnsupported(String)`: mapped when HTTP 400/404/422 returns an error message indicating tool/function calling is not supported by the model or endpoint (e.g., `"does not support tools"`, `"tool_use is not supported"`, `"tools is not supported"`, `"function calling is not supported"`, `"unknown field: tools"`). **Only emitted when `req.tools` is non-empty.**
- `LlmError::ContextOverflow(String)` / `ChatError::ContextOverflow(String)`: mapped when HTTP 400/413/422 reports prompt/context length exceeded (`"context_length_exceeded"`, `"maximum context length"`, `"prompt is too long"`, `"exceeds the maximum number of tokens"`, `"token count exceeds"`, `"input is too long"`).
- Existing `LlmRouter` handling: `ToolsUnsupported` and `ContextOverflow` are **request-level** (`Invalid` semantics): return immediately without marking keys invalid or cooling down, and log usage (`error_kind = "tools_unsupported"` / `"context_overflow"`).
- Note on `non_empty`: An assistant response that has `tool_calls` (even with empty `text`) is **valid** and must NOT trigger `empty response`. Only error with `LlmError::Invalid("empty response")` when both `text.trim().is_empty()` and `tool_calls.is_empty()`.

#### Provider Wire Mapping
1. **OpenAI & OpenAI-Compatible (`openai.rs`):**
   - When `!req.tools.is_empty()`: attach `"tools": [{"type": "function", "function": {"name", "description", "parameters"}}]` and `"tool_choice": "auto"`.
   - Map `Message::Assistant` with `ToolCall`s → `{"role": "assistant", "content": null_or_text, "tool_calls": [{"id", "type": "function", "function": {"name", "arguments": json_string}}]}`.
   - Map `Message::Tool` → one OpenAI `"role": "tool"` message per `ToolResult` (`tool_call_id`, `content`).
   - Parse `choices[0].message.tool_calls` (parsing JSON string arguments safely; malformed arguments fall back to `Value::Null` or string error handled by tool dispatch so the model can self-correct on next turn!) and `finish_reason` (`"tool_calls"` → `StopReason::ToolUse`, `"length"` → `MaxTokens`).
   - Read `usage.prompt_tokens_details.cached_tokens` into `cache_read_tokens`.
2. **Anthropic (`anthropic.rs`):**
   - System prompt sent as array of content blocks with `cache_control: {"type": "ephemeral"}` when `!req.system.is_empty()`.
   - When `!req.tools.is_empty()`: convert `tools` with `"input_schema"`, and attach `"cache_control": {"type": "ephemeral"}` to the **last tool definition**.
   - Additionally, if `messages.len() >= 2`, attach `"cache_control": {"type": "ephemeral"}` to the last content block of the **latest user/tool message** (maximum 3–4 breakpoints total, well within Anthropic's 4-breakpoint limit).
   - Map `Message::Assistant` → content blocks (`{"type": "text", "text"}` and `{"type": "tool_use", "id", "name", "input"}`).
   - Map `Message::Tool` → `"role": "user"` message containing `{"type": "tool_result", "tool_use_id", "content", "is_error"}` blocks (note: Anthropic requires alternating `user`/`assistant` roles; since `Message::Tool` follows `Message::Assistant`, mapping it to `"role": "user"` naturally alternates!).
   - Token accounting: `cache_read = usage.cache_read_input_tokens.unwrap_or(0)`, `cache_write = usage.cache_creation_input_tokens.unwrap_or(0)`, `input_tokens = usage.input_tokens.unwrap_or(0).saturating_add(cache_read).saturating_add(cache_write)`. That way `llm_usage.input_tokens` reflects total input tokens consistently alongside OpenAI and Gemini.
3. **Gemini (`gemini.rs`):**
   - `tools`: `[{"functionDeclarations": [{"name", "description", "parameters"}]}]` + `toolConfig: {"functionCallingConfig": {"mode": "AUTO"}}` when `!req.tools.is_empty()`.
   - Map `Message::Assistant` → `"role": "model"` with `parts` containing `{"text"}` and/or `{"functionCall": {"name", "args"}, "thoughtSignature": ...}` (preserving `provider_meta` when present so Gemini 2.5+/3 thinking models don't reject multi-turn tool responses!).
   - Map `Message::Tool` → `"role": "user"` with `parts` containing `{"functionResponse": {"name": tool_name, "response": {"result": content, "isError": is_error}}}`.
   - Gemini omits call IDs on `functionCall`: assign deterministic synthetic IDs `gc_{prior}_{idx}`, where `prior` is the number of `ToolCall`s already present in `req.messages` and `idx` is the part's index among this response's function calls. IDs are therefore unique across the whole conversation, and `ToolResult.call_id` pairs cleanly.
   - Read `usageMetadata.cachedContentTokenCount` into `cache_read_tokens`.

---

### 4.3 Hardened Ephemeral Git Cloner (`crates/git` + `crates/review/src/workspace.rs`)

#### `CloneSpec` (`crates/git`)
```rust
#[derive(Debug, Clone)]
pub struct CloneSpec {
    pub url: String,
    pub refspec: String,
    /// Tried once when fetching `refspec` fails for a non-auth reason (Azure: the source branch ref).
    pub fallback_refspec: Option<String>,
    pub expected_sha: String,
    /// Value for `Authorization` HTTP header (e.g. `Basic <base64>`), never embedded in `url`, argv or `.git/config`.
    pub auth_header: String,
}
```
- `url` is built with `reqwest::Url` and `path_segments_mut()` so every owner/project/repo segment is percent-encoded (Azure repo names may contain spaces). `clone_spec` returns `None` if any segment is empty.
- `GitProvider::clone_spec(&self, repo: &RepoRef, pr: &PullRequest) -> Option<CloneSpec>` (default `None` so existing fake/test implementations don't break until wired).
- **GitHub (`GithubProvider`):**
  - Maps REST base (`https://api.github.com` → `https://github.com`, or `{origin}/api/v3` → `{origin}` for GHES; `http://` base preserved for local wiremock/test servers if overridden).
  - `url`: `{git_base}/{owner}/{name}.git`, `refspec`: `refs/pull/{pr.number}/head`, `auth_header`: `Basic base64("x-access-token:" + pat)`.
- **Azure DevOps (`AzureProvider`):**
  - Requires `repo.project` (project GUID).
  - `url`: `{base_url}/{organization}/{project}/_git/{name}`, `refspec`: `pr.head_sha.clone()`, `fallback_refspec`: `pr.source_ref.clone()` (the PR's `sourceRefName`, stored on the new `PullRequest.source_ref: Option<String>`; GitHub sets `None`) for servers that refuse fetching a bare SHA, `auth_header`: `Basic base64(":" + pat)`.

#### Workspace & Security Controls (`crates/review/src/workspace.rs`)
- **Trait seam:**
  ```rust
  #[async_trait]
  pub trait RepoCloner: Send + Sync {
      async fn clone_pr(&self, spec: &CloneSpec) -> Result<RepoWorkspace, WorkspaceError>;
  }
  ```
- **RAII ownership (`RepoWorkspace`):**
  - Owns `tempfile::TempDir` created under `<std::env::temp_dir()>/adus-clones/adus-{uuid}/`.
  - Records `canonical_root: PathBuf` (`std::fs::canonicalize(dir.path())`) at creation.
  - Dropping `RepoWorkspace` (normal completion, `EngineError`, timeout, or `tokio::task::JoinHandle::abort()`) removes the directory immediately.
- **Startup orphan sweep:**
  - `cleanup_orphaned_workspaces()` scans `<std::env::temp_dir()>/adus-clones/` on `adus serve` startup (before spawning workers) and deletes stale directories older than 1 hour (or all directories inside `adus-clones/` since `adus serve` owns this process's clone namespace).
- **Hardened `git` invocation (`GitCliCloner`):**
  - Timeout: `CLONE_TIMEOUT = 60 s` (aborts child process with `kill_on_drop(true)`).
  - Env hardening: `env_clear()`, passing only safe minimal vars (`PATH`, `HOME`/`USERPROFILE` pointed to the temp dir or empty, `SystemRoot`/`SYSTEM32` on Windows, plus the operator's proxy and CA variables `http_proxy`/`https_proxy`/`HTTPS_PROXY`/`all_proxy`/`ALL_PROXY`/`no_proxy`/`NO_PROXY`/`GIT_SSL_CAINFO`/`GIT_SSL_CAPATH`/`SSL_CERT_FILE`/`SSL_CERT_DIR`/`CURL_CA_BUNDLE`; never `GIT_TRACE*`/`GIT_CURL_VERBOSE`), `GIT_TERMINAL_PROMPT=0`, `GIT_CONFIG_NOSYSTEM=1`, `GIT_CONFIG_GLOBAL=/dev/null` (or `NUL` on Windows / temp empty config).
  - Step 0: `git --version` once per process (cached); below **2.31** (no `GIT_CONFIG_COUNT` support) → `WorkspaceError::Unavailable("git 2.31 or newer is required")`, so the review falls back to diff-only instead of failing auth.
  - Step 1: `git init` inside temp dir.
  - Step 2: `git -c core.hooksPath=/dev/null -c core.symlinks=false -c core.fsmonitor=false -c filter.lfs.smudge= -c filter.lfs.process= -c protocol.file.allow=never fetch --depth=1 --no-tags --no-recurse-submodules -- <url> <refspec>`. The PAT goes **only** in this command's environment: `GIT_CONFIG_COUNT=1`, `GIT_CONFIG_KEY_0=http.extraHeader`, `GIT_CONFIG_VALUE_0=Authorization: <auth_header>`. It never appears in argv (visible in the process list), the URL, or `.git/config`. If the fetch fails for a non-auth reason and `fallback_refspec` is `Some`, retry the fetch once with it. `--` always precedes `<url> <refspec>`.
  - Step 3: `git -c core.hooksPath=/dev/null -c core.symlinks=false -c core.fsmonitor=false -c filter.lfs.smudge= -c filter.lfs.process= checkout --detach --force FETCH_HEAD`.
  - Verify `HEAD` matches `expected_sha` (warn if mismatch, e.g., PR pushed concurrently mid-clone; do not crash unless empty).
  - Checkout size check: enforce `MAX_WORKSPACE_BYTES = 500 * 1024 * 1024` (500 MB) excluding `.git` during initial index/walk or post-checkout walk; exceed → `WorkspaceError::TooLarge`.
  - Error classification & secret scrubbing: strip any occurrence of `auth_header` / raw PAT from captured stderr. Authentication errors (`Authentication failed`, `401`, `403`, `could not read Username`) → `WorkspaceError::Unauthorized`; timeouts / size / network / non-zero exit → `WorkspaceError::Unavailable(String)`.
- **Validated Path Newtype (`RepoPath`):**
  - Only constructible via `RepoWorkspace::resolve(&self, rel: &str) -> Result<RepoPath, PathError>`.
  - Rejects: empty path, leading `/`, Windows drive prefixes (`C:`), backslashes normalized or rejected (`\`), `..` components, `.git` or `.git/**` components, symlinks escaping `canonical_root` (verified via `canonicalize` against `canonical_root`), and secret files (`is_secret_path`: `.env`, `.env.*` except `.env.example`/`.env.sample`/`.env.template`, `*.pem`, `*.key`, `*.p12`, `*.pfx`, `*.kdbx`, `id_rsa*`, `id_ed25519*`, `.npmrc`, `.netrc`, `secrets.yml`, `secrets.yaml`).

---

### 4.4 Pure-Rust Read-Only Tools & Shared Cache (`crates/review/src/tools.rs`)

Tools execute inside `tokio::task::spawn_blocking` against `Arc<RepoWorkspace>`, with a per-review `Arc<ToolCache>` (`std::sync::RwLock<HashMap<ToolCacheKey, String>>`—never held across `.await`). A `grep`/`find_files` result cut by its time budget is not cached, so a later identical call gets a fresh try.

| Tool | Typed Input (`Deserialize`) | Behavior & Caps | Cached? |
|---|---|---|---|
| `read_file` | `{ path: String, start_line?: u32, end_line?: u32 }` | Validates `RepoPath`; rejects binary files (null byte in first 8 KB) & files > 2 MB; returns `1`-indexed line-numbered text capped at **200 lines / 8,000 chars** per call, with header `[<path>: lines X-Y of Z]` so the agent knows if more lines exist | Yes |
| `grep` | `{ pattern: String, path?: String, glob?: String, case_sensitive?: bool }` | Pure-Rust `ignore::WalkBuilder` (respects `.gitignore`, skips `.git/` & secret files automatically, `follow_links(false)`) + `regex::RegexBuilder` (10 MB size limit to avoid ReDoS). Groups matches with `2` context lines, capped at **40 match groups / 8,000 chars** | Yes |
| `list_dir` | `{ path?: String }` | Validates `RepoPath` (defaults to workspace root); lists immediate children (`dir/` suffix for directories), hides `.git/` & secret files, sorted alphabetically, capped at **150 entries / 4,000 chars** | Yes |
| `find_files` | `{ pattern: String, path?: String }` | Pure-Rust `ignore::WalkBuilder` + `globset::Glob` matching relative paths, skipping `.git/` & secret files, capped at **150 paths / 4,000 chars** | Yes |
| `submit_findings` | `{ findings: Vec<RawFinding> }` | Used by Finder agent loop to terminate cleanly with structured findings (each item validated/parsed leniently per-element like `parse.rs`) | No (terminates loop) |
| `submit_verdict` | `{ keep: bool, rationale: String }` | Used by Verifier agent loop to terminate cleanly with structured verdict | No (terminates loop) |

**Secret redaction (`crates/review/src/redact.rs`):** the successful content of every read-only tool is passed through `redact_secrets` on the blocking thread before it is cached and returned, so cached entries hold the redacted text. Masked as `[REDACTED]`: lines between a private-key PEM `BEGIN`/`END` pair (the marker lines stay; a block cut by a tool window is still masked; after a `BEGIN` with no `END`, only lines that look like a PEM body (base64, `Proc-Type:`/`DEK-Info:` headers) are masked, up to the first other line, so a marker quoted in prose or code leaves the following lines alone; base64 lines right before an `END` with no `BEGIN` are masked too), provider token shapes (AWS access key id, GitHub `gh[pousr]_`/`github_pat_`, `sk-…`, Google `AIza…`, Slack `xox?-`, Stripe `[rs]k_live|test_`, JWT), the password of `scheme://user:password@host`, and the value of a secret-named key (`password`, `passwd`, `secret`, `api_key`/`apikey`/`api-key`, `access_token`, `auth_token`, `client_secret`, `private_key`) assigned a quoted literal of 8+ chars or, on an env-style `KEY=value` line, an unquoted value of 8+ chars. The env-style rule only matches at the start of a line (after a tool line-number prefix and an optional `export `), never an indented line, so keyword arguments and props do not match; a code-shaped value (containing `(`, starting with `{`, or ending in `,` or `)`) is not masked. Placeholders (`${X}`, `<x>`, `{{x}}`, `$X`, `%X`) are left alone. Redaction replaces within a line, never adds or removes one, so line numbers stay correct. The PR diff and the Verifier's `<code_context>` snippet (read raw, outside the cache) are **not** redacted, so a hardcoded secret in the PR can still be found and verified. Finder and Verifier system prompts say: "`[REDACTED]` in tool output marks a value Adus masked because it looked like a secret; a hardcoded secret is still a finding when the diff shows it."

**Tool Error Handling:** Recoverable tool input errors (bad path, missing file, bad regex, secret-file refusal) return `ToolResult { is_error: true, content: "..." }` back to the LLM so the model can adjust its query on the next step rather than aborting the review.

---

### 4.5 Repository Instruction Auto-Loader (`crates/review/src/repo_instructions.rs`)

When `RepoWorkspace` is present, scan a deterministic ordered list of well-known instruction paths inside `workspace` (through `RepoPath` validation):
1. `AGENTS.md`
2. `CLAUDE.md`
3. `.github/copilot-instructions.md`
4. `.cursor/rules/*.mdc` (sorted lexicographically by path)

Limits & Safety:
- Each file is passed through `redact_secrets` (Section 4.4) before it is capped and before it goes into any prompt.
- Cap total concatenated instruction text at **8,000 chars** (`[truncated]` marker when cut).
- Wrapped inside `<repository_instructions>...</repository_instructions>` with closing tags neutralized via the same `neutralize_close_tag` helper in `prompt.rs`.
- Placed in the **user/task prompt** (marked strictly as repository conventions to verify code against—never overriding system instructions, output format, or security constraints).

---

### 4.6 ReAct Agent Loop & Context Compaction (`crates/review/src/agent.rs`)

#### Typed Engine Loop
- `AgentConfig`: `max_steps: usize` (Finder: `20`, Verifier: `6`), `compaction_char_limit: usize` (`160_000`), `max_concurrent_tools: usize` (`4`) (with constant `MAX_OUTPUT_TOKENS: u32 = 8_192`), `wrap_up_steps: usize` (default `1`, Finder `2`), `deadline: Option<tokio::time::Instant>` (default `None`; the Finder sets its stage deadline, the tool-enabled Verifier its own), `token_budget: Option<Arc<TokenBudget>>` (default `None`; the engine passes the run's shared budget, decision 15, and the agent adds each successful response's `input_tokens` to it).
- `AgentOutcome<T>`: `{ result: T, steps_used: usize, cache_read_tokens: u32, cache_write_tokens: u32, cut_short: bool }` (cache tokens summed over every turn, for the completion log).
- `AgentError`: `ToolsUnsupported(String)`, `Chat(ChatError)`, `Unparseable(String)`.

#### Loop Mechanics
1. Send `ChatRequest { model, system, messages, tools, max_tokens, effort }` (`AgentLlm` carries `model` and `effort` for the stage, decision 10).
2. Record assistant message (`text` + `tool_calls`) into `messages`.
3. If `tool_calls` contains the configured terminal submit tool (`submit_findings` or `submit_verdict`):
   - Parse arguments into `T`. If valid → return `Ok(AgentOutcome { result, cut_short: false })`.
   - If malformed arguments → feed `is_error: true` tool result back so the model fixes its JSON on the next step (up to budget).
4. If `tool_calls` is empty (`StopReason::EndTurn` or `MaxTokens`):
   - Attempt prose-JSON fallback parsing from `response.text` (reusing `first_json_array` / JSON extraction). If valid → return `Ok(AgentOutcome { result, cut_short: false })`.
   - Otherwise, if steps remain, append one corrective user prompt asking the model to call the submit tool (max 1 retry), else return `AgentError::Unparseable`.
5. Otherwise, execute the non-submit read-only tool calls concurrently (up to `max_concurrent_tools`), append `Message::Tool { results }`, and advance step counter.
6. **Wrap-up:** Wrap-up mode starts when `steps_used + wrap_up_steps >= max_steps` (the last step, or the last 2 for the Finder; a window larger than `max_steps` makes every step a wrap-up step), **or** when `deadline` is set, at least one step has finished, and the time left is less than 2 × the slowest whole step of this run so far (`WRAP_UP_STEP_FACTOR`; a step is the LLM call plus its tool execution, timed with `tokio::time::Instant`; the first step never triggers it), **or** when `token_budget` is exhausted and at least one step has finished (logged trigger `token_budget`). With 2 ×, at least one such step is left when wrap-up starts, enough for the submit step even under steady latency. Once entered it stays on: read-only tools are **dropped** and only the submit tool is offered (a read-only call still gets the `is_error` "not available" result, so call/result pairing holds), and one reminder ("You are near the end of your budget. Submit your final result now using `<submit>`.", the same for every trigger) is sent when it starts (not right after a nudge; never in `NoTools` runs), so the model outputs its accumulated findings instead of hitting the wall (or the stage deadline) empty-handed. If it still doesn't submit cleanly, the nudge rule still applies once, then partial/unparseable handling (`cut_short: true` for Finder if prose recovers anything, `AgentError` for Verifier). The caller's `timeout_at` stays the hard cut. **Budget note:** with the repository tools and `max_steps >= 4`, one user note is sent before the step where `steps_used == max_steps / 2` (unless wrap-up starts at that step): "Step budget: {used} of {max} steps used. Gather only the evidence you still need, then call {submit}."
7. **Context compaction & overflow recovery:**
   - Before each LLM call, compute total chars across `messages` (tool-call `provider_meta`, which replays thinking blocks, counts too). While `total_chars > compaction_char_limit`, walk from the oldest `Message::Tool` forward (preserving `messages[0]` initial prompt and the **last 4 messages**, i.e., 2 turns) and replace each `ToolResult.content` longer than 120 chars with `"[earlier tool output omitted to save context]"`.
   - If `ChatService::chat` returns `ChatError::ContextOverflow(_)`: perform **one-shot aggressive compaction** (keep only `messages[0]`, the latest assistant message + tool results truncated to 500 chars) and retry once; if it overflows again, surface `AgentError::Chat(ChatError::ContextOverflow(...))`.

---

### 4.7 Prompt Architecture, Batching & Specialists (`crates/review/src/prompt.rs`, `crates/review/src/specialists.rs`)

#### Diff Batching & PR File Manifest
- Build a compact **PR File Manifest** covering every file in the PR diff (`path`, `status`, `+added/-removed` counts, plus `[ignored]`/`[skipped: ...]` notes) so every batch has full PR visibility even when diffs are split.
- Group target files into batches of at most **`40,000` chars** (`MAX_BATCH_CHARS`), sorting by directory prefix so related files (e.g., `crates/db/src/...` or `src/auth/...`) stay together. A single oversized file (up to `max_patch_lines_per_file`) gets its own solo batch.

#### Prompt Ordering (Prefix-Cache Optimization)
System prompt places **static shared blocks first**, followed by the specialist/mode-specific `<Role>` at the tail:
1. `<ReviewInstructions>` — core high-recall defect-hunt mindset, 3-phase workflow (*Investigate callers/definitions → Challenge edge cases/races/invariants → Respond via `submit_findings`*), scope rules (`+` lines, cross-file anchoring on the changed trigger line), tool discipline (don't reread unchanged lines without a new question), untrusted-input rules, severity definitions.
2. `<Role>` — mode/specialist focus (`Generalist`, `Bug`, `Security`, `Performance`, or `Rules`).

User prompt (`messages[0]`):
1. `<pr_title>`, `<pr_description>` (4,000 chars cap), `<pr_manifest>`
2. `<rules>` (only rules matching at least one file in the batch, labeled with their `path_glob` and severity), `<memories>`, `<repository_instructions>` (when present)
3. `<diffs>` (annotated patches with right-side line numbers for all files in the batch)

In **self-contained diff-only fallback mode**, `<ReviewInstructions>` omits tool-calling steps and instructs single-turn JSON/submit output using only `<pr_manifest>` and `<diffs>`.

#### Specialist Roster (`specialists.rs`)
- `SpecialistKind`: `Generalist | Bug | Security | Performance | Rules` (`Copy`, `Eq`, `Hash`).
- **Concurrency:** batches run up to `BATCH_CONCURRENCY = 2` at a time; all specialists of one batch run in parallel (so at most 8 Finder agents in `Specialists` mode, 2 in `Generalist` mode).
- **`Generalist` mode:** runs `[SpecialistKind::Generalist]` per diff batch.
- **`Specialists` mode:** runs `[Bug, Security, Performance]` in parallel per batch, **plus** `SpecialistKind::Rules` *only if* at least one enabled rule matches any file in that batch (`rules_for_batch` non-empty). `Bug`/`Security`/`Performance` exclude pure convention/style noise (focusing on their respective domains), while `Rules` focuses strictly on team rules + repository instructions (`AGENTS.md`, etc.).

---

### 4.8 Verifier & Semantic Deduplication (`crates/review/src/verifier.rs`, `crates/review/src/dedup.rs`)

#### Step 1: Deterministic Filter (`parse.rs`)
- Add `keep_valid_batch(suggestions: Vec<Suggestion>, allowed_by_file: &HashMap<String, &CommentableRange>) -> Vec<Suggestion>`. `CommentableRange { lines: BTreeSet<u32>, hunks: Vec<RangeInclusive<u32>> }` is built from the patch (`commentable_lines`, `commentable_hunks`); in incremental reviews both are the intersection of the commit diff and the PR diff (`intersect_hunks`).
  - Normalizes `s.file` (`normalize_path`) and keeps only findings whose file is in the batch and whose `s.line` is in `lines`.
  - Keeps `s.start_line` only when `0 < start_line < line` and both lie in one hunk of `hunks` (`same_hunk`). When a supplied `start_line` is dropped, the finding is kept single-line and its `suggestion` is removed, because a multi-line replacement anchored to one line duplicates code on "Apply suggestion" (§4.12).
  - Deduplicates exact `(file, line, title)` tuples; when two duplicates differ only in range, the one with a valid `start_line` wins.

#### Step 2: Per-Finding Tool-Enabled Verifier (`verifier.rs`)
- **Cap & prioritization:** If more than `MAX_VERIFY_CANDIDATES = 25` candidates survive deterministic filtering, sort by severity (`Critical > Warning > Info`, preserving original order on ties), verify the top 25, and record a summary warning (`"N lower-priority findings exceeded the verification cap and were omitted"`).
- **Model:** the provider's `effective_fast_model()` with `ReasoningEffort::Low` (decision 10). The fast model may lack tool support even when the primary model has it: a `ToolsUnsupported` from the Verifier switches the remaining candidates to the diff-only path below.
- **Concurrency:** Run up to `VERIFY_CONCURRENCY = 4` verifier tasks concurrently using `futures::stream::iter(...).buffer_unordered(VERIFY_CONCURRENCY)` (preserving deterministic ordering afterwards).
- **Evidence bundle per candidate:**
  - The candidate finding JSON (`file`, `start_line` when set, `line`, `severity`, `title`, `body`, `suggestion`); the snippet covers the whole `start_line..=line` span
  - Surrounding lines / patch hunk for the cited file
  - Applicable rules for that file + team memories + repo instructions (if present)
  - Order in the user prompt: rules, memories, repo instructions, diff, code context, then the candidate and the task line, so candidates on the same file share a cacheable prefix (decision 10)
- **Verification prompt rules:**
  - Skeptical surgical verifier: **refute-to-drop, keep-by-default**.
  - Must actively check definitions, types, callers, upstream guards, or tests via `read_file`/`grep`/`list_dir`/`find_files` (max `6` steps) before dropping.
  - Drop **only** if concrete evidence refutes the claim (upstream guard guarantees safety, factually misread code/imports, unreachable branch, pure style/nitpick not backed by a team rule or repo instruction, or generic "missing X" with no failure path).
  - Never rewrite finding text, line, or severity.
- **Diff-only fallback:** When `Toolbox` is unavailable (clone failed or `ToolsUnsupported`), and for every candidate that starts once the review's token budget is exhausted (decision 15), run 1 single-turn Verifier call per candidate (same prompt rules minus tool instructions) against the file's annotated patch + rules + memories.
- **Fail-closed:** Any verifier error (LLM error, deadline, `Unparseable`, aborted wrap-up) **drops** that candidate and increments `dropped_unverified` for the summary warning.
- **Retryable exception:** If `candidates > 0`, **no** candidate got a verdict (kept or refuted), and **every** verifier failure was retryable (`ChatError::AllKeysFailed`, `NoKeys` or `Internal`), the report carries `all_retryable_error` and `Engine` returns `EngineError::Retryable`. The row is deleted and the queue retries the whole job, so a rate-limit spike is never posted as "No issues found." (The retry repeats the clone and Finder cost; this is accepted.) A deadline cut is never retryable: those candidates are dropped as unverified.

#### Step 3: PR-Wide Semantic Deduplication (`dedup.rs`)
- Runs **only when `verified.len() >= 2`**.
- One single-turn `ChatRequest::text` call (`effective_fast_model()`, `ReasoningEffort::Low`) listing all surviving findings indexed `0..N` (with `file`, `start_line` when set, `line`, `severity`, `title`, `body`, `specialist`).
- Instructs the model to identify semantic duplicates (findings describing the same underlying root cause at the same or related location—especially common when `Bug` and `Security` both flag the same flaw) and return JSON `{"keep_indices": [0, 2, ...]}` selecting the single clearest/highest-severity finding per root cause.
- **Validation & safe fallback:** Validate indices are in bounds `0..N` and unique (reject empty `keep_indices` if input was non-empty!). On any LLM error, parse error, or invalid output, log a warning and keep all `verified` findings unchanged.

---

### 4.9 Observability, Summary Formatting & Doctor

- **Summary comment (`format_summary`):**
  - Retains `<!-- adudu -->`, severity breakdown (`Critical`, `Warning`, `Info`), `"No issues found."`, `N files ignored`, and `**Skipped files**`.
  - Adds optional `**Notes**` / `warnings: &[String]` section (e.g., `"Codebase context unavailable — reviewed diff only"`, `"Performance reviewer did not complete"`, `"2 candidate findings could not be verified and were omitted"`, `"5 lower-priority findings exceeded the verification cap and were omitted"`).
- **Secret redaction on posting:** after Dedup and before `post_review`, every inline comment body and the summary body go through `redact_secrets` (Section 4.4). A finding whose `suggestion` contains a secret (or an already masked `[REDACTED]` value) is posted without its suggestion block, keeping its line span, so "Apply suggestion" never writes `[REDACTED]` into code; its body ends with "_Suggested code omitted: it contained a value that looks like a secret._". One `debug!` per review logs `redacted_tool_outputs` and `redacted_comments` (summary included), never the matched text.
- **Structured logging:** Log `review_mode`, `clone_ms`, `fallback_reason`, per-specialist `steps_used` / `candidates`, `verified_kept`, `verifier_dropped`, `dedup_dropped`, total `cache_read_tokens` / `cache_write_tokens`, and the run's `input_tokens` (the token budget's count) and `token_budget_reached` on completion. When the ceiling changed something (decision 15) the summary adds the note "Token budget reached ({used} of {limit} input tokens): {details}."; Verifier candidates it switched to diff-only are named there ("N findings verified from the diff only"), never dropped.
- **`Dockerfile` & `adus doctor`:**
  - `Dockerfile`: install `git` alongside `ca-certificates` (`apt-get install -y --no-install-recommends ca-certificates git`).
  - `crates/server/src/doctor.rs`: add `check_git_binary()` running `git --version`; if missing, unexecutable or older than 2.31, emit `Status::Warn` ("`git` 2.31+ not found — reviews will run in diff-only fallback mode without full codebase context") so `adus doctor` surfaces it on bare-metal setups without blocking `NotRunning`.

### 4.11 Time Budget

A review's budget is chosen once its effective Finder mode is known (command override → repo → settings, then the small-PR force to Generalist): `specialists_review_timeout` (`SPECIALISTS_REVIEW_TIMEOUT`, 30 min in production) for Specialists, else `review_timeout` (`REVIEW_TIMEOUT`, 12 min; small PRs and `@adudu review generalist` included). It counts from the start of the run (fetches and clone included); once settings are read, the rest of the run is bounded by the budget it can still end with (Generalist: `review_timeout`; Specialists: the larger of both, since a small PR may still be forced to Generalist), and the run is cut at `start + budget` of its effective mode (`Fatal("review timed out")`, row `failed`), and `Engine::review_pr`'s outer `tokio::time::timeout` uses the larger of the two budgets only as a backstop. The worker chain is `JOB_TIMEOUT` 31 min < `LOCK_TTL` 35 min, with the queue take-over (`RECLAIM_IDLE`) and `reviews::STALE_AFTER` at 35 min. An agentic review of a large PR could exceed its budget and end `failed` with nothing posted, so each stage gets its own deadline, computed from the run's start as fractions of the budget (so short test timeouts scale too):

| Stage | Deadline | When it is hit |
|---|---|---|
| Clone + Finder | `start + 60 %` | A clone still running is abandoned and the review continues diff-only (`fallback_reason = "clone_timed_out"`). Finder tasks still running are cancelled (`tokio::time::timeout_at`) and count as failed specialists with the note `"{label} reviewer did not finish in time"`; the files of a batch no agent finished are listed under **Skipped files** (`timed_out`, "review did not finish in time"). A deadline cut is never retryable, also not under the Generalist retry rule of decision 8. If **no** Finder task finished, the review fails `Fatal("review timed out")`, as today. |
| Verifier | `start + 90 %` | Candidates without a verdict are dropped as unverified (fail-closed, never retryable). |
| Dedup | `start + 97 %` (the last 3 % of the run's budget, ≈ 22 s for 12 min and ≈ 54 s for 30 min, is reserved for posting) | Dedup is cut and falls back safely (keep the verified list). |

Reasoning makes each call slower: a request with an effort set gets a 300 s per-request timeout instead of 120 s, and its output cap grows by a reserve for reasoning tokens (provider-models spec §3.2). The stage deadlines above still bound the whole run; `Specialists` mode at `High` effort (~60 s per step observed live) is the case most likely to hit the Finder deadline, which is why it gets the 30 min budget and why Finder and Verifier agents wrap up early when their deadline is closer than 2 × their slowest step, LLM call plus tools (Section 4.6 step 6). An agent cut anyway ends in a partial review with notes, never a silent timeout.

Cancelling an agent future drops its pending tool futures; `spawn_blocking` work already started runs to completion against its `Arc<RepoWorkspace>` clone. The directory is removed when the last `Arc` drops (at the latest when `run()` ends or is cancelled by the outer timeout).

---

### 4.10 API & Dashboard UI (`crates/server` & `web/`)

- **Settings (`GET`/`PUT /api/v1/settings`):**
  - Add `review_mode: "generalist" | "specialists"` (`SettingsJson` & optional `SettingsBody.review_mode: Option<ReviewMode>` so partial callers without the field remain backwards compatible).
  - Add `small_pr_max_lines: i32` (optional in `SettingsBody`, absent keeps the value; outside `0..=10000` is a 422 `validation_error`) and a **Small PR threshold (changed lines)** number field (Section 4.13).
  - Add `max_review_input_tokens: i32` (decision 15; optional in `SettingsBody`, absent keeps the value; outside `0..=100000000` is a 422 `validation_error` "max_review_input_tokens must be between 0 and 100000000") and a **Max input tokens per review** number field. Defaults after migration 0008: `max_files_per_review` 30, `max_patch_lines_per_file` 1500, `small_pr_max_lines` 100, `max_review_input_tokens` 2,000,000.
- **Usage (`GET /api/v1/usage/daily`, `/usage/breakdown`):** rows carry `cache_read_tokens` and `cache_write_tokens` (sums, zero-filled). The Token Usage page adds "· {n} cache read" to the totals line and a right-aligned **Cache read** breakdown column.
  - UI (`web/src/routes/settings/+page.svelte`): add a **Review mode** `<Select>` ("Generalist (balanced)" / "Specialists (deep: Bug, Security, Performance & Rules)") with helper copy mentioning `@adudu review deep`.
- **Repositories (`GET /api/v1/repositories`, `PATCH /api/v1/repositories/{id}`):**
  - Add `review_mode: Option<ReviewMode>` (`null` = inherit global default) to `RepositoryJson` and `UpdateRepository` (using `present_or_null` so `null` clears override).
  - UI (`web/src/routes/repositories/+page.svelte`): add a **Review mode** table column (`Select` for admin: `Default (global)`, `Generalist`, `Specialists`; read-only label for `user` role, matching `DESIGN.md` and `#lib` import conventions).

---

### 4.12 Multi-Line Inline Comments (`crates/git` + `crates/review`)

Added 2026-11-22 (user decision). A multi-line ` ```suggestion ` fence anchored to a single line makes GitHub's "Apply suggestion" insert duplicate lines; the comment must cover the span it replaces.

- **Meaning of the span:** `start_line..=line` is the span of **existing** new-file lines (RIGHT side) that the `suggestion` replaces. The Finder sets `start_line` only when that span has more than one line; an insertion or a finding whose suggestion rewrites only `line` leaves it unset. The Finder prompt and the `submit_findings` schema say this (schema stays in the Gemini-safe subset).
- **Parsing:** `start_line` is optional and lenient (number or numeric string); `start_line >= line` or `0` means single-line (`None`).
- **Validation:** §4.8 Step 1 — kept only when `start_line` and `line` lie in one hunk of the file's `CommentableRange` (incremental reviews use the intersected hunks). A dropped `start_line` also removes the finding's `suggestion` (the finding is still posted, single-line, without a committable fence).
- **Verifier & Dedup:** both see `start_line` when set; the Verifier's code snippet covers the whole span; the Verifier prompt says what the span means.
- **Posting:** `InlineComment` carries optional `start_line` and `end_offset`.
  - GitHub: when `start_line < line`, the review comment adds `start_line` and `start_side: "RIGHT"` next to `line` and `side: "RIGHT"`; otherwise both are omitted.
  - Azure DevOps: thread context `rightFileStart {line: start_line, offset: 1}` and `rightFileEnd {line, offset: end_offset}`, where `end_offset` is the UTF-16 length of the end line + 1 (`right_line_end_offset`); single-line threads keep `offset: 1` on both ends. `end_offset` is ignored by GitHub.
  - Providers re-check `start_line < line` and fall back to single-line.
- **Unverified offline:** live GitHub same-hunk acceptance and Apply behaviour, and Azure offset semantics — user click-test.

### 4.13 Small PR: Diff-Only Review (`crates/review/src/small_pr.rs`)

Added 2026-11-22 (user decision). A small, low-risk PR does not need a clone or an agent that walks the repository, so it takes the cheap path.

- **Predicate:** `is_small_pr(targets, rules, max_lines)` over the **reviewed targets only** (ignored and skipped files never count; an all-ignored PR never gets here). True only when all hold:
  - `max_lines > 0` (`settings.small_pr_max_lines`, default 100, range `0..=10000`; `0` = never);
  - `1 <= targets.len() <= SMALL_PR_MAX_FILES` (3);
  - changed lines (`+` and `-` inside hunks, summed over the targets) `<= max_lines`;
  - no target is on a **risky path**, checked on `normalize_path`, case-insensitive, for both `path` and `old_path` (renames):
    - security/access: any path segment containing `auth`, `security`, `crypto`, `permission`, `secret`, `token`, `session` or `password`;
    - data/schema: under `migrations/`, `*.sql`, file stem `schema`;
    - shared contracts: `*.proto`, `openapi.*`, `*.graphql`, file name `lib.rs`, `mod.rs`, `index.ts`, `index.js`;
    - build/deploy: `Dockerfile*`, `.github/workflows/**`, `Cargo.toml`, `package.json`;
  - no target is covered (`rule_matches_path`) by an enabled `critical` rule (the same applicable-rules list the engine batches with).
- **Engine:** decided in `review_targets` before the clone. When small: no `clone_spec` / `clone_pr`, no `Toolbox`, so the Finder runs with `with_tools = false` and the Verifier runs diff-only (the existing diff-only path of Section 4.8, reused, not a second path). The mode is forced to `Generalist` after normal resolution (Section 4.1), whatever the repo or global mode, so the Finder is one call per batch with `finder_effort(Generalist)`. The Verifier and Dedup still run.
- **Command bypass:** `Trigger::Command` (`@adudu review`, `@adudu review deep`) never takes this path: it clones, uses tools and honours the requested mode.
- **Not a failure:** the summary note is `Small change — reviewed diff only` (not `Codebase context unavailable`). Logs carry `fallback_reason = "small_pr"` and the effective `review_mode`. Finder failure handling is unchanged (Generalist still retries when every failure is retryable, decision 8).

---

## 5. Rust Engineering Standards (Applying `/rust-best-practices`)

For this wave, Adus deliberately moves from the original "minimal Rust-learner subset" in `AGENTS.md` to idiomatic production Rust following Apollo GraphQL's Rust Best Practices:

1. **Type-state / newtype invariants:** `RepoPath` can only be constructed through `RepoWorkspace::resolve`; `RepoWorkspace` binds directory lifetime to RAII `Drop`.
2. **Borrowing over cloning:** Accept `&str`, `&[T]`, and `Cow<'_, str>` at API boundaries; pass small `Copy` enums (`ReviewMode`, `Severity`, `SpecialistKind`) by value; avoid `.clone()` inside hot search/agent loops.
3. **Closed enums & static dispatch:** Use enums (`SpecialistKind`, `ToolKind`) internally; keep `dyn Trait` strictly at external/test seams (`GitProvider`, `GitProviderFactory`, `LlmClient`, `ChatService`, `RepoCloner`, `JobRunner`).
4. **Explicit error hierarchies:** `thiserror` enums (`WorkspaceError`, `ToolError`, `AgentError`, extended `LlmError` / `ChatError`) with zero `.unwrap()` or `.expect()` in library/production code.
5. **Typed serde schemas:** All tool parameters (`ReadFileArgs`, `GrepArgs`, `ListDirArgs`, `FindFilesArgs`, `SubmitFindingsArgs`, `SubmitVerdictArgs`) and LLM verdict/dedup outputs parse into concrete `Deserialize` structs rather than ad-hoc `Value` indexing.
6. **Clean async boundaries:** Synchronous directory walks, file I/O, and regex scans execute inside `tokio::task::spawn_blocking`; `std::sync::RwLock` guards `ToolCache` and is never held across `.await`.
7. **Strict clippy & tests:** `cargo clippy --workspace --all-targets -- -D warnings` (watching `redundant_clone`, `large_enum_variant`, `needless_collect`; document any needed suppression with `#[expect(clippy::..., reason = "...")]`), deterministic offline tests using local `TempDir` git fixtures and wiremock servers.
