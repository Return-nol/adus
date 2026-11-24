# Reasoning Effort Wiring & Reasoning Replay — Design Specification

Date: 2026-11-24 · Status: implemented (consolidated into [docs/SPEC.md](../../SPEC.md) §6.1)

> **Binding scope & policy:** Extends [docs/SPEC.md](../../SPEC.md) §6.1 and the provider spec ([2026-11-21-provider-models-and-reasoning-design.md](2026-11-21-provider-models-and-reasoning-design.md)). The clean-room and no-assumption rule in `AGENTS.md` applies: Kodus community code may be read as a behaviour reference only (cite it), Kodus enterprise code (`libs/ee/**`, `*.ee.*`) is never opened. Wire formats come from public provider docs. All code is written fresh for Adus.

---

## 1. Intent & Why

`ReasoningEffort` (`Low`/`Medium`/`High`) is Adus's main quality lever for reviews, but it does not reach several providers as intended:

1. **Anthropic.** Every request sends `thinking: {type: "enabled", budget_tokens}`. Current Claude models (Opus 5.5 / 5 / 4.8 / 4.7, Sonnet 5.5 / 5, Haiku 5.5, Fable / Mythos 5.x) reject `budget_tokens` with a 400 and Opus / Sonnet 4.6 deprecate it. The effort fallback then retries with no effort: the configured level is ignored, every call costs a failed round-trip, and the output cap loses its reasoning reserve while the model still thinks.
2. **Gemini.** Every request sends `thinkingBudget`. Gemini 3 and later take `thinkingLevel`.
3. **Reserves are small.** 2,048 / 4,096 / 8,192 output tokens. On OpenAI-compatible endpoints reasoning counts against `max_tokens` (OpenRouter docs), and DeepSeek thinks at `high` by default, so a Finder call at `Medium` (8,192 + 4,096) can end in "model exhausted max_tokens during reasoning".
4. **OpenAI-compatible reasoning is not replayed.** Adus reads `reasoning` / `reasoning_content` only to detect an exhausted cap. DeepSeek requires every earlier assistant turn's `reasoning_content` back whenever the request has `tools`, otherwise 400 — and every Finder and Verifier loop sends tools.

**Success:** each provider gets the effort control it understands; DeepSeek (direct or through OpenRouter) completes multi-step tool loops; reasoning-heavy calls stop running out of output tokens.

## 2. Goals, Non-Goals

### Goals

1. Anthropic: adaptive thinking plus `output_config.effort` for Claude 4.6 and later; `budget_tokens` only for older Claude models and non-Claude ids (§3).
2. Gemini: `thinkingLevel` for Gemini 3 and later; `thinkingBudget` otherwise (§4).
3. Reserves: `Low 4,096 / Medium 12,000 / High 23,000` (§5).
4. OpenAI-compatible: replay the reasoning fields an endpoint returned, on every later assistant turn (§6).

### Non-Goals

- A per-model "thinking off" switch or a reasoning-traits table beyond the id rules below.
- Anthropic `display: "summarized"`, task budgets, per-message effort, server-side fallbacks, or the newer `anthropic-version` header.
- Gemini thought signatures on text-only parts (Adus keeps them on function-call parts only). Revisit if live reviews show `MISSING_THOUGHT_SIGNATURE`.
- Compaction changes. Agent compaction edits old tool results; on Claude models with preserved thinking this may drop earlier thinking blocks. To verify live; out of scope here.
- Any setting, migration, API or UI change.

## 3. Anthropic

### 3.1 Model rule

`adaptive_thinking(model: &str) -> bool` (private, `anthropic.rs`):

- No `claude-` substring → `false` (Anthropic-compatible proxies keep today's behaviour).
- Contains `claude-3` → `false`.
- `claude-{opus|sonnet|haiku}-{major}[(-|.)minor]…`, where `minor` is a 1–2 digit token separated by `-` or `.` (an 8-digit date or `latest` is not a minor): `false` when `major < 4`, or `major == 4` and `minor` (absent = 0) `≤ 5`; otherwise `true`.
- Any other `claude-…` id (Fable, Mythos, future names) → `true`.

| Model id | Mode |
|---|---|
| `claude-opus-5-5`, `claude-sonnet-5-5`, `claude-sonnet-5.5`, `claude-haiku-5-5`, `claude-fable-5-2`, `claude-opus-4-6`, `claude-opus-4.6`, `claude-sonnet-4-6`, `claude-opus-4-8` | adaptive |
| `claude-haiku-4-5`, `claude-sonnet-4-5`, `claude-sonnet-4.5`, `claude-opus-4-5-20251101`, `claude-opus-4-1`, `claude-sonnet-4-20250514`, `claude-3-7-sonnet-latest`, `kimi-k2` | budget |

### 3.2 Request body (effort `Some(e)`)

- **Adaptive:** `"thinking": {"type": "adaptive"}`, `"output_config": {"effort": e.as_str()}`, `max_tokens = req.max_tokens + e.output_reserve()`.
- **Budget:** unchanged shape, `{"type": "enabled", "budget_tokens": e.output_reserve()}`, same `max_tokens`.
- Effort `None`: neither field (unchanged).
- Thinking-block replay through `ToolCall.provider_meta` is unchanged.

### 3.3 Fallback

- `REASONING_UNSUPPORTED_MARKERS` gains `"output_config"`, so a proxy that rejects the new field falls back to no effort as today.
- **Cap-overflow fallback (`effort.is_some()`):** if a non-404 `LlmError::Invalid` message names `max_tokens`, `max_completion_tokens`, or `maxoutputtokens` (case-insensitive) alongside an excess/range cue (`exceed`, `maximum`, `at most`, `must be <=`, `less than`, `too large`, or ` > `), `chat_with_effort_fallback` treats it as reasoning-reserve incompatibility and retries once with `effort: None` (restoring `req.max_tokens` without the reserve). This protects non-reasoning 8K/16K-capped models (`gpt-4o`, non-thinking OpenAI-compatible servers) that validate `max_tokens` before checking `reasoning_effort`.

## 4. Gemini

- `thinking_level(model: &str) -> bool`: parses the **full multi-digit decimal integer** immediately after `gemini-` (stopping at `.`, `-`, or end of string), returning `true` when `N ≥ 3` (covering Gemini 3, Gemini 4 such as `gemini-4-pro`, and multi-digit versions like `gemini-10-pro`). Ids without a leading version (aliases, `m-1`) and `gemini-2.5-*` → `false`.
- **Level** (Gemini 3+ / Gemini 4+): `"thinkingConfig": {"thinkingLevel": L}` with `Low → "LOW"`, `Medium → "MEDIUM"`, `High → "HIGH"`; for ids containing `gemini-3-pro-preview`, `Medium → "HIGH"` (that model accepts only low and high).
- **Budget** (otherwise): `"thinkingConfig": {"thinkingBudget": e.output_reserve()}` — the separate `thinking_budget()` table is removed.
- Both: `maxOutputTokens = req.max_tokens + e.output_reserve()`.

## 5. Reserves

`ReasoningEffort::output_reserve`: `Low 4,096`, `Medium 12,000`, `High 23,000`. With `MAX_OUTPUT_TOKENS = 8,192`, `High` totals `31,192 ≤ 32,000` (staying inside 32K-capped models such as `claude-opus-4-1`) and `23,000` stays inside every Gemini 2.5 `thinkingBudget` range Kodus encodes (§9, incl. Flash's `24,576` ceiling), so no clamping is needed while Gemini 4 and Claude 5.5 get generous headroom. `REASONING_REQUEST_TIMEOUT` (300 s) is unchanged. A reserve is a cap: providers bill only the tokens used.

## 6. OpenAI-Compatible Reasoning Replay

### 6.1 Types (`types.rs`)

- `AssistantPart::ProviderMeta(String)` — opaque per-turn provider data (a JSON object), sent back verbatim by the client that produced it and ignored by the others.
- `ChatResponse.provider_meta: Option<String>` — that data for the turn just answered. Anthropic and Gemini always return `None`.

### 6.2 Capture (`openai.rs`)

`WireMessage` also reads `reasoning_details: Option<Value>`. A helper (`capture_provider_meta`) inspects `reasoning_content` (present when `!s.trim().is_empty()`), `reasoning_details` (present when `as_array().is_some_and(|a| !a.is_empty())`), and `reasoning` (present when `!s.trim().is_empty()`). When at least one is present, `provider_meta` is the JSON object of exactly those present fields under their own names, e.g. `{"reasoning_content":"…"}` or `{"reasoning":"…","reasoning_details":[…]}`. Text-only turns are captured too.

For exhaustion detection, `has_reasoning` is derived directly as `provider_meta.is_some()` (removing the separate `has_reasoning` check), so OpenRouter models returning only `reasoning_details` also report `"empty response: model exhausted max_tokens during reasoning"` when `finish_reason == "length"` with empty text and tool calls.

### 6.3 Replay (`openai.rs` `assistant_message`)

For an assistant message holding a `ProviderMeta` part that parses as a JSON object, only keys in the explicit allowlist `["reasoning_content", "reasoning_details", "reasoning"]` are copied onto the outgoing assistant message (so malformed or unexpected metadata can never overwrite `"role"`, `"content"`, or `"tool_calls"`). A part that does not parse is skipped. Native `Openai` never returns these fields, so it never replays anything.

### 6.4 Review engine (`crates/review/src/agent.rs`)

`assistant_message` stores the response's `provider_meta` as the first part of the assistant message; `message_char_len` counts it, so compaction sees its size (reasoning parts themselves are never compacted).

## 7. Risks

- An OpenAI-compatible server that returns a reasoning field but rejects it on input would now fail tool loops. None known; the OpenRouter guide lists `reasoning_content` as an input alias of `reasoning`.
- Replayed reasoning grows each request's input on DeepSeek; the token ceiling and compaction still bound it.
- DeepSeek maps `medium` to `high` (DeepSeek docs); Adus sends the word as is.
- Anthropic's raw HTTP `/v1/messages` accepts `max_tokens > 21,333` without streaming, but some SDK-based proxies enforce a streaming check above ~21,333 tokens (which would only affect `Specialists` / `High` routed through such a proxy).

## 8. Testing

- `crates/llm/tests/clients.rs`: adaptive body for a current Claude id; legacy body with new reserves (existing test updated); `output_config` rejection falls back; `max_tokens` / `max_completion_tokens` cap-exceeded rejection with `effort: High` falls back to `effort: None`; Gemini 3/4 level (incl. `gemini-3-pro-preview` Medium → `HIGH`, `gemini-4-pro`, `gemini-10-pro`) and 2.5 budget (existing test updated); OpenAI-compatible capture and replay incl. text-only turns, whitespace-only / empty-array omission, `reasoning_details` exhaustion detection, and replay key allowlist; `ProviderMeta` never reaches Anthropic or Gemini bodies; existing OpenAI reserve assertion updated.
- `anthropic.rs` unit test: the §3.1 table (including dot-delimited `claude-sonnet-4.5`, `claude-opus-4.6`, `claude-sonnet-5.5`).
- `crates/review/src/agent.rs`: the assistant message keeps `provider_meta` and resends it.

## 9. Sources

- Anthropic thinking and effort rules: the `claude-api` skill's thinking table (adaptive on 4.6+, `budget_tokens` 400 on Opus 4.7+ / 5.x, Sonnet 5 / 5.5, Haiku 5.5, Fable 5.x; `output_config.effort`).
- Google AI Studio thinking docs (`thinkingLevel` for Gemini 3+, error on earlier models; per-model levels, `gemini-3-pro-preview` low / high only). Gemini 2.5 budget ranges are not stated there.
- OpenRouter reasoning-tokens guide and parameters page (`reasoning_effort` accepted, reasoning counts against `max_tokens`, `reasoning_details` response field, `reasoning_content` input alias).
- DeepSeek thinking-mode guide (`reasoning_effort`, thinking on by default, `reasoning_content` must be passed back when `tools` are sent).
- Kodus community code, behaviour only: `libs/llm/providers/kernel/effort-budget.ts` (budget table, used only for older Claude / Gemini 2.5), `libs/llm/providers/anthropic/{index,traits}.ts` (adaptive for Claude 4.6+), `libs/llm/providers/google-gemini/{index,reasoning}.ts` (Gemini 3 level, 2.5 budget ranges: pro 128–32,768, flash ≤ 24,576, flash-lite 512–24,576), `libs/llm/providers/kernel/reasoning-traits.ts` (DeepSeek traits). Kodus does not replay OpenAI-compatible reasoning itself; Adus's replay rule is its own.

## 10. Assumptions

- Gemini `thinkingLevel` values are sent as the enum names `LOW` / `MEDIUM` / `HIGH`.
- Unversioned Gemini aliases keep `thinkingBudget`.
- Echoing all three reasoning fields as received is safe on OpenRouter and DeepSeek.

## 11. SPEC.md Sync

§6.1 "Provider quirks" (Anthropic `-`/`.` adaptive rule, Gemini 3+/4+ multi-digit `thinkingLevel` rule, OpenAI-compatible allowlisted replay & `reasoning_details` exhaustion detection) and "Reasoning effort" (new reserves `4,096 / 12,000 / 23,000`, `output_config` marker, `max_tokens` cap-overflow fallback); §10 entry for this spec.
