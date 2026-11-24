# Reasoning Effort Wiring & Reasoning Replay Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `ReasoningEffort` reach Anthropic and Gemini in the form each model accepts, raise the reasoning reserves, and replay OpenAI-compatible reasoning so DeepSeek tool loops work.

**Architecture:** Two pure id rules (`adaptive_thinking`, `thinking_level`) pick each provider's request shape; one `output_reserve` table drives every cap and budget. Reasoning returned by OpenAI-compatible endpoints rides on a new opaque `AssistantPart::ProviderMeta` part, filled from `ChatResponse.provider_meta` by the review agent and echoed only by the OpenAI client.

**Tech Stack:** Rust, `serde_json`, `wiremock` client tests in `crates/llm/tests/clients.rs`, `crates/review` agent loop.

**Spec:** [docs/superpowers/specs/2026-11-24-reasoning-effort-wiring-and-replay-design.md](../specs/2026-11-24-reasoning-effort-wiring-and-replay-design.md)

## Global Constraints

- **Clean-room and no-assumption rule (binding, verbatim from `AGENTS.md`):**
  1. **Kodus community code** (https://github.com/kodustech/kodus-ai, anything outside `libs/ee/**` and not named `*.ee.*`) may be read as a behaviour reference and inspiration. Never port or translate it line by line: Adus code, prompts, and schemas are written fresh. **Kodus enterprise code** (`libs/ee/**`, `*.ee.*`) is never opened, fetched, or searched, not even to look.
  2. **Order of authority:** the task brief (or active unmerged change spec), then [docs/SPEC.md](docs/SPEC.md) (and [DESIGN.md](docs/DESIGN.md) for `web/`), then this file and the existing Adus code. Kodus community behaviour (or public provider/git docs for wire formats) only fills a gap none of those settles. Never guess from memory of how Kodus or another tool works; if a gap remains, raise it instead of assuming.
  3. **Openness:** every implementer report lists its assumptions under an `"Assumptions"` heading and cites each Kodus community file consulted (path) and the behaviour taken from it.
  4. **Reviewers** flag as **Important** any line-by-line port or translation, anything that looks derived from Kodus enterprise code, uncited Kodus use, unstated assumptions beyond the brief/spec, redundant unit tests, and missing [docs/SPEC.md](docs/SPEC.md) updates when SDD completes.
  5. **Plans and prompts:** every new plan repeats this rule in its Global Constraints, and every implementer and reviewer subagent prompt carries it verbatim.
- Exact values: `output_reserve` `Low 4_096`, `Medium 12_000`, `High 23_000`; new marker `"output_config"` + `max_tokens` cap-overflow fallback (`effort.is_some()` + non-404 `Invalid` matching `max_tokens`/`max_completion_tokens`/`maxoutputtokens` with an excess/range cue); Gemini levels `"LOW"` / `"MEDIUM"` / `"HIGH"`, `gemini-3-pro-preview` Medium → `"HIGH"`; replay key allowlist `["reasoning_content", "reasoning_details", "reasoning"]`.
- No setting, migration, API or `web/` change; `anthropic-version` stays `2023-06-01`.
- Zero `.unwrap()`/`.expect()` outside tests; no new `dyn Trait`; no new dependency (no `regex` for the id rules — split Claude minor versions on `-` or `.`, and parse the full multi-digit integer after `gemini-`).
- Branch: `fix/llm-effort-wiring` from the current `HEAD`. **Precondition:** the user's uncommitted edits (`.cargo/config.toml`, `.github/workflows/release.yml`, `Dockerfile`, `crates/review/tests/engine.rs`) are not part of this work and are never staged; `crates/review/tests/engine.rs` must be committed or stashed by the user before Task 3, which edits it.
- Independent of `docs/superpowers/plans/2026-11-24-finder-completion-gate-and-verifier-effort.md`; land this plan first (`crates/review/src/agent.rs` overlaps cleanly if that plan rebases onto this).
- Commands: `cargo test -p adus-llm -j 2`, `cargo test -p adus-review -j 2`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all`.
- Commits: Conventional Commits ending with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`. Never push.

## Review Focus

1. Dated, aliased, dot-delimited, and proxy model ids (`claude-sonnet-4-20250514`, `claude-sonnet-4.5`, `claude-opus-4-5-20251101`, `claude-3-7-sonnet-latest`, `kimi-k2`) → keep `budget_tokens`; `claude-fable-5-2`, `claude-opus-4-6`, `claude-opus-4.6`, `claude-sonnet-5.5` → adaptive. Pinned in Task 2 (`adaptive_thinking_follows_model_generation`).
2. Non-reasoning or lower-capped model rejects raised `max_tokens` / `max_completion_tokens` before checking `reasoning_effort` → falls back once to `effort: None` (without reserve). Pinned in Task 1 (`retries_without_effort_when_reserve_exceeds_model_max_tokens`).
3. Endpoint returns whitespace-only `"reasoning_content": "   "` or `"reasoning_details": []` → no `ProviderMeta`, nothing echoed; malformed `ProviderMeta` cannot overwrite `"role"`/`"content"`/`"tool_calls"`. Pinned in Task 3 (`openai_compatible_replays_returned_reasoning_fields`).
4. DeepSeek answers a turn with text only (no tool call) → that turn's reasoning is still replayed; `reasoning_details`-only length exhaustion reports `"model exhausted max_tokens during reasoning"`. Pinned in Task 3 (`openai_compatible_replays_returned_reasoning_fields`).
5. A proxy rejects `output_config` → retry without effort succeeds; unversioned Gemini alias (`m-1`, `gemini-flash-latest`) keeps `thinkingBudget` while `gemini-4-pro` / `gemini-10-pro` get `thinkingLevel`. Pinned in Task 2 (`anthropic_adaptive_falls_back_when_output_config_rejected`) and Task 1 (`gemini_3_plus_sends_thinking_level`).

---

### Task 1: Reserves, cap-overflow fallback, and Gemini thinking mode

**Files:**
- Modify: `crates/llm/src/types.rs:41-47` (`output_reserve`), `:231-255` (`is_reasoning_unsupported` / `is_max_tokens_exceeded`, `chat_with_effort_fallback`)
- Modify: `crates/llm/src/gemini.rs` (`thinking_budget` removed, `chat_once` ~105-114, new private fns)
- Test: `crates/llm/tests/clients.rs` (`retries_without_effort_when_provider_rejects_reasoning_param` ~1308, `gemini_sends_thinking_budget_and_filters_thought_parts` ~1413, new tests)

**Interfaces:**
- Produces: `ReasoningEffort::output_reserve` (`Low 4_096`, `Medium 12_000`, `High 23_000`); `chat_with_effort_fallback` retrying on reasoning-unsupported markers or raised `max_tokens` cap rejection; private `fn thinking_level(model: &str) -> bool` and `fn thinking_config(model: &str, effort: ReasoningEffort) -> Value` in `gemini.rs`.

- [x] **Step 1: Update and add tests**
  - `retries_without_effort_when_provider_rejects_reasoning_param`: `first["max_completion_tokens"] == 100 + 23_000`.
  - New `retries_without_effort_when_reserve_exceeds_model_max_tokens`: first OpenAI mock returns 400 `{"error": {"message": "max_completion_tokens exceeds maximum of 16384"}}` when `max_completion_tokens == 8192 + 23000`; second returns a text reply. Send `ChatRequest::text("gpt-4o", "sys", "hi", 8192).with_effort(High)`; assert 2 requests, second has `max_completion_tokens == 8192` and no `reasoning_effort`.
  - `gemini_sends_thinking_budget_and_filters_thought_parts` (model `m-1`, `Low`): `generationConfig == {"maxOutputTokens": 200 + 4096, "thinkingConfig": {"thinkingBudget": 4096}}`.
  - New `gemini_3_plus_sends_thinking_level`: for each `(model, effort, level)` in `[("gemini-3-flash-preview", Medium, "MEDIUM"), ("gemini-3-pro-preview", Medium, "HIGH"), ("gemini-3.1-pro-preview", High, "HIGH"), ("gemini-3-flash-preview", Low, "LOW"), ("gemini-4-pro", Medium, "MEDIUM"), ("gemini-10-pro", Low, "LOW")]`, serve `/v1beta/models/{model}:generateContent`, send `ChatRequest::text(model, "sys", "hi", 200).with_effort(effort)`, assert `generationConfig == {"maxOutputTokens": 200 + effort's reserve, "thinkingConfig": {"thinkingLevel": level}}`. Add budget cases `("gemini-2.5-flash", High)` → `{"thinkingBudget": 23000}` and `("gemini-flash-latest", Low)` → `{"thinkingBudget": 4096}` (unversioned alias).

- [x] **Step 2: Run to verify they fail**

Run: `cargo test -p adus-llm -j 2 --test clients -- gemini_ retries_without_effort`
Expected: FAIL (old reserve values, no `thinkingLevel`, no cap-overflow fallback).

- [x] **Step 3: Implement** — new reserve values (`4_096`, `12_000`, `23_000`); add `is_max_tokens_exceeded(err)` (non-404 `Invalid` matching `max_tokens`/`max_completion_tokens`/`maxoutputtokens` paired with `exceed`/`maximum`/`at most`/`must be <=`/`less than`/`too large`/` > `) to `chat_with_effort_fallback` in `types.rs`; `thinking_level` per spec §4 (parse the full multi-digit integer immediately after `gemini-`, stopping at `.`/`-`/end); `thinking_config` returns the spec §4 object; `chat_once` uses `effort.output_reserve()` for `maxOutputTokens` and `thinking_config` for `thinkingConfig`. Update the doc comment of `output_reserve` to name the values' use as Gemini 2.5 / legacy Claude budgets.

- [x] **Step 4: Run** `cargo test -p adus-llm -j 2` — PASS.

- [x] **Step 5: Lint, format, commit**

```bash
git add crates/llm/src/types.rs crates/llm/src/gemini.rs crates/llm/tests/clients.rs
git commit -m "fix(llm): raise reasoning reserves, add cap fallback, and send thinkingLevel to Gemini 3+"
```

---

### Task 2: Anthropic adaptive thinking

**Files:**
- Modify: `crates/llm/src/anthropic.rs` (`chat_once` ~88-100, new private fn, `#[cfg(test)] mod tests` if none)
- Modify: `crates/llm/src/types.rs:223` (`REASONING_UNSUPPORTED_MARKERS` gains `"output_config"`; array length 6)
- Test: `crates/llm/tests/clients.rs` (`anthropic_sends_thinking_budget_and_replays_thinking_blocks_on_tool_turns` ~1371-1373, two new tests)

**Interfaces:**
- Consumes: `output_reserve` (Task 1).
- Produces: private `fn adaptive_thinking(model: &str) -> bool` in `anthropic.rs`.

- [x] **Step 1: Write the tests**
  - Unit `adaptive_thinking_follows_model_generation` in `anthropic.rs`: `true` for `claude-opus-5-5`, `claude-sonnet-5-5`, `claude-sonnet-5.5`, `claude-haiku-5-5`, `claude-fable-5-2`, `claude-opus-4-6`, `claude-opus-4.6`, `claude-sonnet-4-6`, `claude-opus-4-8`; `false` for `claude-haiku-4-5`, `claude-sonnet-4-5`, `claude-sonnet-4.5`, `claude-opus-4-5-20251101`, `claude-opus-4-1`, `claude-sonnet-4-20250514`, `claude-3-7-sonnet-latest`, `kimi-k2`, `m-1`.
  - Update the existing legacy test (`claude-sonnet-4-5`, `Medium`): `thinking == {"type": "enabled", "budget_tokens": 12000}`, `max_tokens == 8192 + 12000`, and `first_body.get("output_config").is_none()`.
  - New `anthropic_current_model_sends_adaptive_thinking_and_effort`: `ChatRequest::text("claude-opus-5-5", "sys", "hi", 8192).with_effort(High)`; body `thinking == {"type": "adaptive"}`, `output_config == {"effort": "high"}`, `max_tokens == 8192 + 23000`, and the serialized body does not contain `budget_tokens`.
  - New `anthropic_adaptive_falls_back_when_output_config_rejected`: first mock (matching `body_partial_json({"output_config": {"effort": "medium"}})`, `up_to_n_times(1)`) returns 400 `{"type": "error", "error": {"type": "invalid_request_error", "message": "output_config: Extra inputs are not permitted"}}`; second returns a text reply. Assert 2 requests, the second has neither `thinking` nor `output_config`, and `max_tokens == 100` for `ChatRequest::text("claude-opus-5-5", "sys", "hi", 100).with_effort(Medium)`.

- [x] **Step 2: Run to verify they fail**

Run: `cargo test -p adus-llm -j 2 -- adaptive anthropic_`
Expected: FAIL / compile error (`adaptive_thinking` missing).

- [x] **Step 3: Implement** `adaptive_thinking` per spec §3.1 (split the part after `claude-` on `-` or `.`; a minor is a 1–2 digit token) and the spec §3.2 bodies; add the marker.

- [x] **Step 4: Run** `cargo test -p adus-llm -j 2` — PASS.

- [x] **Step 5: Lint, format, commit**

```bash
git add crates/llm/src/anthropic.rs crates/llm/src/types.rs crates/llm/tests/clients.rs
git commit -m "fix(llm): use adaptive thinking and output_config effort for Claude 4.6+"
```

---

### Task 3: Replay OpenAI-compatible reasoning

**Files:**
- Modify: `crates/llm/src/types.rs` (`AssistantPart`, `ChatResponse`)
- Modify: `crates/llm/src/openai.rs` (`WireMessage` ~53, `capture_provider_meta`, response build ~188, `assistant_message` ~251)
- Modify: `crates/llm/src/anthropic.rs` (~278-280 wire match, response ~171), `crates/llm/src/gemini.rs` (~278-281 wire match, response ~186)
- Modify: `crates/review/src/agent.rs` (~239 destructuring, `assistant_message` ~489, `message_char_len` ~556, test match ~736, new test)
- Modify (new field only): `crates/review/src/test_support.rs` (~177, ~189), `crates/review/tests/engine.rs` (~340), `crates/review/tests/learn.rs` (~67), `crates/llm/tests/router.rs`, `crates/llm/tests/clients.rs` where a `ChatResponse` is built
- Test: `crates/llm/tests/clients.rs`

**Interfaces:**
- Consumes: nothing from Tasks 1–2.
- Produces: `AssistantPart::ProviderMeta(String)`; `ChatResponse.provider_meta: Option<String>`; `fn assistant_message(text: &str, calls: &[ToolCall], provider_meta: Option<String>) -> Message` in `agent.rs`.

- [x] **Step 1: Write the failing tests**
  - `openai_compatible_replays_returned_reasoning_fields` (`OpenaiCompatible`):
    1. reply `{"content": null, "reasoning_content": "check caller", "tool_calls": [read_file call_1]}` → `resp.provider_meta` parses to `{"reasoning_content": "check caller"}`;
    2. reply `{"content": "done", "reasoning": "r", "reasoning_details": [{"type": "reasoning.text", "text": "r"}]}` (text only) → parses to `{"reasoning": "r", "reasoning_details": [...]}`;
    3. reply `{"content": "ok", "reasoning_content": "   ", "reasoning_details": []}` → `provider_meta == None`;
    4. reply `{"content": null, "reasoning_details": [{"type": "reasoning.text", "text": "thinking..."}]}` with `finish_reason: "length"` → `Err(LlmError::Invalid("empty response: model exhausted max_tokens during reasoning"))`.
    Then send a request whose history has assistant turn A = `[ProviderMeta(meta1), ToolCall(call_1)]` + its tool result, text-only assistant turn B = `[ProviderMeta(meta2), Text("done")]` + a user message, and turn C = `[ProviderMeta("{\"reasoning_content\":\"ok\",\"role\":\"system\"}"), Text("x")]` + a user message; assert sent turn A has `"reasoning_content": "check caller"`, turn B has `"reasoning"` and `"reasoning_details"` equal to what was received, and turn C replays `"reasoning_content": "ok"` while keeping `"role": "assistant"`.
  - `provider_meta_part_is_never_sent_to_anthropic_or_gemini`: `turn_two()` with `AssistantPart::ProviderMeta("{\"reasoning_content\":\"x\"}".into())` prepended to the assistant parts; for `Anthropic` and `Gemini`, the sent body does not contain `"reasoning_content"` and the assistant message equals the one sent without the part.
  - `agent.rs` `provider_meta_is_kept_and_resent`: first scripted turn is `tools_turn(vec![read("c1", "src/a.rs")])` with `provider_meta = Some(r#"{"reasoning_content":"t"}"#.into())` set on the `Ok` value, then `submit("c2", "done")`; assert the second request's `messages[1] == Message::Assistant { parts: vec![AssistantPart::ProviderMeta(r#"{"reasoning_content":"t"}"#.into()), AssistantPart::ToolCall(read("c1", "src/a.rs"))] }`.

- [x] **Step 2: Run to verify they fail**

Run: `cargo test -p adus-llm -j 2 --test clients -- replays provider_meta` and `cargo test -p adus-review -j 2 --lib -- provider_meta`
Expected: compile errors (`ProviderMeta`, `provider_meta` missing).

- [x] **Step 3: Implement** per spec §6: the two type additions; capture in `openai.rs` via `capture_provider_meta` (`reasoning_details: Option<Value>` on `WireMessage`, keep only `!s.trim().is_empty()` for `reasoning`/`reasoning_content` and non-empty JSON arrays for `reasoning_details`) and set `has_reasoning = provider_meta.is_some()`; echo in `assistant_message` (copy only allowlisted keys `["reasoning_content", "reasoning_details", "reasoning"]` from the parsed object; skip a part that does not parse); Anthropic and Gemini skip the part in their wire mapping and set `provider_meta: None`; in `agent.rs` keep `provider_meta` from the response, push `AssistantPart::ProviderMeta` first in `assistant_message`, count its chars in `message_char_len`, and add the arm to the test match; add `provider_meta: None` to every other `ChatResponse` literal.

- [x] **Step 4: Run** `cargo test -p adus-llm -j 2` and `cargo test -p adus-review -j 2` — PASS.

- [x] **Step 5: Lint, format, commit**

```bash
git add crates/llm crates/review/src/agent.rs crates/review/src/test_support.rs crates/review/tests/engine.rs crates/review/tests/learn.rs
git commit -m "fix(llm): replay OpenAI-compatible reasoning on later assistant turns"
```

---

### Task 4: Sync `docs/SPEC.md` and mark spec implemented

**Files:**
- Modify: `docs/SPEC.md` §6.1, §10
- Modify: `docs/superpowers/specs/2026-11-24-reasoning-effort-wiring-and-replay-design.md` (status line)

- [x] **Step 1: Update §6.1** — "Provider quirks": Anthropic `-`/`.` adaptive rule and `output_config.effort` vs `budget_tokens`; Gemini 3+/4+ multi-digit `thinkingLevel` vs `thinkingBudget`; OpenAI-compatible `reasoning_content` / `reasoning` / `reasoning_details` capture into `ChatResponse.provider_meta`, `reasoning_details` exhaustion detection, and allowlisted replay via `AssistantPart::ProviderMeta`. "Reasoning effort": reserves `4,096 / 12,000 / 23,000` (also the Gemini 2.5 / legacy Claude budgets), marker `output_config`, and `max_tokens` cap-overflow fallback.
- [x] **Step 2: Add the §10 row** — `| 2026-11-24 | [2026-11-24-reasoning-effort-wiring-and-replay-design.md](superpowers/specs/2026-11-24-reasoning-effort-wiring-and-replay-design.md) | Consolidated (adaptive Claude thinking, Gemini 3+/4+ thinkingLevel, larger reasoning reserves + cap fallback, OpenAI-compatible reasoning replay) |`.
- [x] **Step 3: Mark the spec** `Status: implemented (consolidated into [docs/SPEC.md](../../SPEC.md) §6.1)`.
- [x] **Step 4: Final gates** — `cargo test --workspace -j 2`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all -- --check`: all pass.
- [x] **Step 5: Commit**

```bash
git add docs/SPEC.md docs/superpowers/specs/2026-11-24-reasoning-effort-wiring-and-replay-design.md docs/superpowers/plans/2026-11-24-reasoning-effort-wiring-and-replay.md
git commit -m "docs: sync SPEC.md for reasoning effort wiring and replay"
```
