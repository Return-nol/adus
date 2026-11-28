//! Live trace of every LLM call a review makes, for debugging prompts and model behaviour.
//!
//! Off by default. `adus serve --trace` (or `ADUS_TRACE=true`) turns this module's target,
//! `adus_review::trace`, on at `TRACE`; `RUST_LOG=...,adus_review::trace=trace` does the same.
//! Each call traces its prompt, then per step the model's reasoning, reply text and tool calls,
//! and each tool result (cut to [`RESULT_CHARS`]). Every traced text is secret-redacted; source
//! code the model read is not, so a trace is a debugging aid, not a production log.

use adus_llm::{ChatResponse, ToolResult};

use crate::agent::TRUNCATED_MARKER;
use crate::prompt::truncate_with_marker;
use crate::redact::redact_secrets;

/// Most characters of one tool result shown in the trace.
const RESULT_CHARS: usize = 2_000;

fn enabled() -> bool {
    tracing::enabled!(tracing::Level::TRACE)
}

/// Traces the system prompt and the first user message of a call. `call` names a single-turn
/// call (`"dedup"`, `"severity"`, ...); agent runs pass `None` and are named by their
/// `finder` / `verify` span instead.
pub(crate) fn prompt(call: Option<&'static str>, system: &str, prompt: &str) {
    if !enabled() {
        return;
    }
    tracing::trace!(
        call,
        system = %redact_secrets(system),
        prompt = %redact_secrets(prompt),
        "agent prompt"
    );
}

/// Traces what the model produced in one step: reasoning, reply text and every tool call.
pub(crate) fn response(step: usize, response: &ChatResponse) {
    if !enabled() {
        return;
    }
    if let Some(reasoning) = response.reasoning() {
        tracing::trace!(step, reasoning = %redact_secrets(&reasoning), "agent reasoning");
    }
    if !response.text.trim().is_empty() {
        tracing::trace!(step, text = %redact_secrets(&response.text), "agent text");
    }
    for call in &response.tool_calls {
        let args = call.arguments.to_string();
        tracing::trace!(
            step,
            tool = %call.name,
            id = %call.id,
            args = %redact_secrets(&args),
            "agent tool call"
        );
    }
}

/// Traces the result of every tool call of one step, each cut to [`RESULT_CHARS`].
pub(crate) fn results(step: usize, results: &[ToolResult]) {
    if !enabled() {
        return;
    }
    for result in results {
        let content = truncate_with_marker(&result.content, RESULT_CHARS, TRUNCATED_MARKER);
        tracing::trace!(
            step,
            tool = %result.tool_name,
            id = %result.call_id,
            is_error = result.is_error,
            content = %redact_secrets(&content),
            "agent tool result"
        );
    }
}
