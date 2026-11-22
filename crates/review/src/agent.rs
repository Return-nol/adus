//! Reusable ReAct agent loop: the model calls read-only repository tools until it submits its
//! result through one terminal tool. Context compaction and a one-shot context-overflow
//! recovery keep long runs within the model's window. Used by the Finder and the Verifier.

use std::sync::Arc;
use std::time::Duration;

use adus_llm::router::{ChatError, ChatService, UsageContext};
use adus_llm::{
    AssistantPart, ChatRequest, ChatResponse, Message, ReasoningEffort, ToolCall, ToolDef,
    ToolResult, UserPart,
};
use futures::{StreamExt, stream};
use serde_json::Value;
use tokio::time::Instant;
use uuid::Uuid;

use crate::budget::TokenBudget;
use crate::tools::{ToolOutput, Toolbox};

/// Default `AgentConfig::compaction_char_limit`.
pub const DEFAULT_COMPACTION_CHAR_LIMIT: usize = 160_000;
/// Default `AgentConfig::max_output_tokens`.
pub const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 8_192;
/// Default `AgentConfig::max_concurrent_tools`.
pub const DEFAULT_MAX_CONCURRENT_TOOLS: usize = 4;
/// Default `AgentConfig::wrap_up_steps`: only the final step is a wrap-up step.
pub const DEFAULT_WRAP_UP_STEPS: usize = 1;

/// Replaces old tool output when the conversation grows past the compaction limit.
pub const OMITTED_TOOL_OUTPUT: &str = "[earlier tool output omitted to save context]";
/// Only tool outputs longer than this are compacted.
const COMPACT_MIN_CHARS: usize = 120;
/// The newest messages (two turns) are never compacted.
const COMPACT_KEEP_TAIL: usize = 4;
/// A run with a `deadline` wraps up once the time left is less than this many times its
/// slowest whole step (LLM call plus tools). With 2, at least one such step is left when
/// wrap-up starts, enough for the submit step.
pub const WRAP_UP_STEP_FACTOR: u32 = 2;
/// The halfway budget note is only sent to runs with at least this many steps.
const BUDGET_NOTE_MIN_STEPS: usize = 4;
/// Tool outputs kept after a context overflow are cut to this many chars.
const OVERFLOW_TOOL_CHARS: usize = 500;
/// Appended to a tool output cut after a context overflow.
const TRUNCATED_MARKER: &str = "\n[output truncated]";

/// Limits of one agent run.
#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// LLM calls allowed in one run (Finder 20, Verifier 6).
    pub max_steps: usize,
    /// Old tool outputs are compacted while the conversation is longer than this (chars).
    pub compaction_char_limit: usize,
    pub max_output_tokens: u32,
    /// Read-only tool calls of one turn that run at the same time.
    pub max_concurrent_tools: usize,
    /// The last steps that offer only the terminal tool (Finder 2, otherwise 1). 0 counts as 1.
    pub wrap_up_steps: usize,
    /// When set, the run wraps up early once the time left is less than
    /// [`WRAP_UP_STEP_FACTOR`] × its slowest step so far (LLM call plus tools), so it submits
    /// before a caller's hard cut at this instant.
    pub deadline: Option<Instant>,
    /// The review's shared input-token budget. Every successful response adds its
    /// `input_tokens`; once the budget is exhausted the run wraps up (after its first step).
    pub token_budget: Option<Arc<TokenBudget>>,
}

impl AgentConfig {
    /// `max_steps` with the default limits for everything else.
    pub fn new(max_steps: usize) -> Self {
        Self {
            max_steps,
            compaction_char_limit: DEFAULT_COMPACTION_CHAR_LIMIT,
            max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
            max_concurrent_tools: DEFAULT_MAX_CONCURRENT_TOOLS,
            wrap_up_steps: DEFAULT_WRAP_UP_STEPS,
            deadline: None,
            token_budget: None,
        }
    }
}

/// A finished run.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentOutcome<T> {
    pub result: T,
    /// LLM calls made (the overflow retry repeats a step and is not counted twice).
    pub steps_used: usize,
    pub cache_read_tokens: u32,
    pub cache_write_tokens: u32,
    /// The step budget ran out and `result` was recovered from the last reply's prose.
    pub cut_short: bool,
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum AgentError {
    /// The model or endpoint rejected tool calling; the caller falls back to diff-only.
    #[error("tools not supported: {0}")]
    ToolsUnsupported(String),
    #[error("{0}")]
    Chat(ChatError),
    /// No valid result was submitted. The text never contains prompt or PR content.
    #[error("agent result unparseable: {0}")]
    Unparseable(String),
}

/// The terminal tool that ends a run, and how its result is read.
pub trait SubmitSpec: Send + Sync {
    type Output: Send;

    /// Definition sent to the model (for example `submit_findings`).
    fn tool_def(&self) -> &ToolDef;

    /// Reads the terminal tool's arguments. The error text is shown to the model.
    fn parse_arguments(&self, arguments: &Value) -> Result<Self::Output, String>;

    /// Recovers the result from a reply that did not call the terminal tool.
    fn parse_prose(&self, text: &str) -> Result<Self::Output, String>;
}

/// Which tools the model gets.
#[derive(Debug, Clone, Copy)]
pub enum AgentTools<'a> {
    /// Read-only repository tools plus the terminal tool.
    Full(&'a Toolbox),
    /// Only the terminal tool (no checkout, but the model supports tools).
    SubmitOnly,
    /// No tools at all (the model rejected tool calling); results come from prose.
    NoTools,
}

/// Where and how the agent talks to the model.
#[derive(Clone, Copy)]
pub struct AgentLlm<'a> {
    pub chat: &'a dyn ChatService,
    pub provider_id: Uuid,
    pub model: &'a str,
    pub system: &'a str,
    pub effort: Option<ReasoningEffort>,
    /// Cloned into every `chat` call.
    pub usage: &'a UsageContext,
}

/// Runs the agent on `prompt` (the first user message) until it submits a result.
///
/// Every step is one LLM call. A reply without tool calls is read with `parse_prose` (and
/// nudged once if that fails), and when the budget runs out the last reply's prose is tried
/// once more (`cut_short`).
///
/// **Wrap-up:** from the last `wrap_up_steps` steps on, or earlier once `deadline` is closer
/// than 2 × the slowest whole step of this run (never before one step was measured), or once
/// the review's `token_budget` is exhausted (also not before the first step), only the
/// terminal tool is offered, and one reminder to submit is sent when that starts. Halfway
/// through a budget of at least 4 steps with the repository tools, one note tells the model
/// how many steps it has used.
///
/// Cancel-safe: all state is local to the future, so dropping it (a deadline) ends the run.
/// Tool work already handed to `spawn_blocking` finishes on its own.
pub async fn run_react_loop<S: SubmitSpec>(
    llm: &AgentLlm<'_>,
    tools: AgentTools<'_>,
    submit: &S,
    config: &AgentConfig,
    prompt: String,
) -> Result<AgentOutcome<S::Output>, AgentError> {
    let submit_def = submit.tool_def();
    let submit_name = submit_def.name.as_str();
    let mut messages = vec![user_message(prompt)];
    let mut tally = Tally::default();
    let mut overflow_retried = false;
    let mut nudged = false;
    let mut nudged_last_step = false;
    let mut last_text = String::new();
    let mut wrap_up = WrapUp::default();

    while tally.steps_used < config.max_steps {
        let starts_wrap_up = wrap_up.check(tally.steps_used, config);
        let with_repo_tools = matches!(tools, AgentTools::Full(_));
        // A nudge sent just before already asks for the result; one request is enough.
        if starts_wrap_up && !nudged_last_step && !matches!(tools, AgentTools::NoTools) {
            messages.push(user_message(format!(
                "You are near the end of your step or time budget. Submit your final result now using {submit_name}."
            )));
        } else if with_repo_tools
            && !wrap_up.active()
            && config.max_steps >= BUDGET_NOTE_MIN_STEPS
            && tally.steps_used == config.max_steps / 2
        {
            messages.push(user_message(format!(
                "Step budget: {} of {} steps used. Gather only the evidence you still need, then call {submit_name}.",
                tally.steps_used, config.max_steps
            )));
        }
        nudged_last_step = false;
        let read_only = match tools {
            AgentTools::Full(toolbox) if !wrap_up.active() => Some(toolbox),
            _ => None,
        };
        let offered = offered_tools(tools, read_only, submit_def);

        // The whole step is timed (LLM call plus tools): the slowest one sizes the margin a
        // deadline-close run keeps for its submit step.
        let step_started = Instant::now();
        compact(&mut messages, config.compaction_char_limit);
        let response = request(
            llm,
            &mut messages,
            offered,
            config.max_output_tokens,
            &mut overflow_retried,
        )
        .await?;
        tally.add(&response);
        if let Some(budget) = &config.token_budget {
            budget.add(response.input_tokens);
        }
        tracing::debug!(
            step = tally.steps_used,
            tools = ?response.tool_calls.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            stop_reason = ?response.stop_reason,
            wrap_up = wrap_up.reason,
            "agent step"
        );
        let ChatResponse {
            text, tool_calls, ..
        } = response;
        messages.push(assistant_message(&text, &tool_calls));

        if tool_calls.is_empty() {
            if let Ok(result) = submit.parse_prose(&text) {
                return Ok(tally.finish(result, false));
            }
            if nudged || tally.steps_used >= config.max_steps {
                return Err(unparseable(submit_name, tally.steps_used));
            }
            nudged = true;
            nudged_last_step = true;
            messages.push(user_message(format!(
                "Please submit your result by calling {submit_name} (or return the JSON directly)."
            )));
            wrap_up.record(step_started.elapsed());
            continue;
        }

        // The first valid submit ends the run; other calls of that turn are not executed.
        let mut submit_errors = Vec::with_capacity(tool_calls.len());
        for call in &tool_calls {
            if call.name != submit_name {
                submit_errors.push(None);
                continue;
            }
            match submit.parse_arguments(&call.arguments) {
                Ok(result) => return Ok(tally.finish(result, false)),
                Err(err) => submit_errors.push(Some(err)),
            }
        }
        let results = run_tool_calls(
            &tool_calls,
            submit_errors,
            read_only,
            submit_name,
            config.max_concurrent_tools,
        )
        .await;
        messages.push(Message::Tool { results });
        last_text = text;
        wrap_up.record(step_started.elapsed());
    }

    match submit.parse_prose(&last_text) {
        Ok(result) => Ok(tally.finish(result, true)),
        Err(_) => Err(unparseable(submit_name, tally.steps_used)),
    }
}

/// When a run stops investigating and only submits (see `run_react_loop`).
#[derive(Default)]
struct WrapUp {
    /// Why wrap-up started (`"steps"`, `"deadline"` or `"token_budget"`); once set it stays set.
    reason: Option<&'static str>,
    /// The slowest whole step (LLM call plus tools) of this run so far.
    slowest: Option<Duration>,
}

impl WrapUp {
    fn active(&self) -> bool {
        self.reason.is_some()
    }

    /// Called before each step; returns true when wrap-up starts at this step.
    fn check(&mut self, steps_used: usize, config: &AgentConfig) -> bool {
        if self.active() {
            return false;
        }
        // At least the final step always wraps up.
        let window = config.wrap_up_steps.max(1);
        self.reason = if steps_used.saturating_add(window) >= config.max_steps {
            Some("steps")
        } else if self.deadline_close(config.deadline) {
            Some("deadline")
        } else if steps_used > 0
            && config
                .token_budget
                .as_ref()
                .is_some_and(|budget| budget.exhausted())
        {
            Some("token_budget")
        } else {
            None
        };
        self.active()
    }

    /// The time left is less than [`WRAP_UP_STEP_FACTOR`] × the slowest step, so one more
    /// investigating step could leave too little time to submit. False until one step was
    /// measured.
    fn deadline_close(&self, deadline: Option<Instant>) -> bool {
        let (Some(deadline), Some(slowest)) = (deadline, self.slowest) else {
            return false;
        };
        let needed = slowest.saturating_mul(WRAP_UP_STEP_FACTOR);
        deadline.saturating_duration_since(Instant::now()) < needed
    }

    fn record(&mut self, step: Duration) {
        self.slowest = Some(self.slowest.map_or(step, |slowest| slowest.max(step)));
    }
}

/// Steps and cache tokens of one run.
#[derive(Default)]
struct Tally {
    steps_used: usize,
    cache_read_tokens: u32,
    cache_write_tokens: u32,
}

impl Tally {
    fn add(&mut self, response: &ChatResponse) {
        self.steps_used += 1;
        self.cache_read_tokens = self
            .cache_read_tokens
            .saturating_add(response.cache_read_tokens);
        self.cache_write_tokens = self
            .cache_write_tokens
            .saturating_add(response.cache_write_tokens);
    }

    fn finish<T>(&self, result: T, cut_short: bool) -> AgentOutcome<T> {
        AgentOutcome {
            result,
            steps_used: self.steps_used,
            cache_read_tokens: self.cache_read_tokens,
            cache_write_tokens: self.cache_write_tokens,
            cut_short,
        }
    }
}

/// Safe error text: names the terminal tool and the step count, nothing else.
fn unparseable(submit_name: &str, steps_used: usize) -> AgentError {
    AgentError::Unparseable(format!(
        "no valid {submit_name} result after {steps_used} steps"
    ))
}

/// Tool definitions for one request. `read_only` is the toolbox when this step may use it.
fn offered_tools(
    tools: AgentTools<'_>,
    read_only: Option<&Toolbox>,
    submit_def: &ToolDef,
) -> Vec<ToolDef> {
    match (tools, read_only) {
        (AgentTools::NoTools, _) => Vec::new(),
        (_, Some(toolbox)) => {
            let mut defs = toolbox.read_only_tool_defs().to_vec();
            defs.push(submit_def.clone());
            defs
        }
        (_, None) => vec![submit_def.clone()],
    }
}

/// One LLM call. A context overflow is retried once per run on a compacted conversation, as
/// the same step.
async fn request(
    llm: &AgentLlm<'_>,
    messages: &mut Vec<Message>,
    tools: Vec<ToolDef>,
    max_tokens: u32,
    overflow_retried: &mut bool,
) -> Result<ChatResponse, AgentError> {
    let build = |messages: &[Message], tools: Vec<ToolDef>| ChatRequest {
        model: llm.model.to_owned(),
        system: llm.system.to_owned(),
        messages: messages.to_vec(),
        tools,
        max_tokens,
        effort: llm.effort,
    };
    let first = llm
        .chat
        .chat(
            llm.provider_id,
            build(messages, tools.clone()),
            llm.usage.clone(),
        )
        .await;
    match first {
        Err(ChatError::ContextOverflow(_)) if !*overflow_retried => {
            *overflow_retried = true;
            tracing::debug!("context overflow; retrying once with a compacted conversation");
            compact_for_overflow(messages);
            llm.chat
                .chat(llm.provider_id, build(messages, tools), llm.usage.clone())
                .await
                .map_err(agent_error)
        }
        other => other.map_err(agent_error),
    }
}

fn agent_error(err: ChatError) -> AgentError {
    match err {
        ChatError::ToolsUnsupported(message) => AgentError::ToolsUnsupported(message),
        other => AgentError::Chat(other),
    }
}

/// One result per call, in call order. Read-only calls run concurrently when `read_only` is
/// available; otherwise they get an error telling the model to submit.
async fn run_tool_calls(
    calls: &[ToolCall],
    submit_errors: Vec<Option<String>>,
    read_only: Option<&Toolbox>,
    submit_name: &str,
    max_concurrent: usize,
) -> Vec<ToolResult> {
    // Futures are built up front: a closure inside the stream would make the run future
    // lose `Send` (higher-ranked lifetime inference).
    let pending: Vec<_> = calls
        .iter()
        .zip(submit_errors)
        .map(|(call, submit_error)| answer_call(call, submit_error, read_only, submit_name))
        .collect();
    stream::iter(pending)
        // `buffered` keeps the results in call order; 0 would never poll anything.
        .buffered(max_concurrent.max(1))
        .collect()
        .await
}

/// The result for one call of a turn that did not end the run.
async fn answer_call(
    call: &ToolCall,
    submit_error: Option<String>,
    read_only: Option<&Toolbox>,
    submit_name: &str,
) -> ToolResult {
    let output = match (submit_error, read_only) {
        (Some(err), _) => ToolOutput {
            content: format!(
                "Error: invalid {submit_name} arguments: {err}. Call {submit_name} again with corrected arguments."
            ),
            is_error: true,
        },
        (None, Some(toolbox)) => toolbox.execute_read_only(&call.name, &call.arguments).await,
        (None, None) => ToolOutput {
            content: format!(
                "Error: tool {} is not available now; call {submit_name} with your result",
                call.name
            ),
            is_error: true,
        },
    };
    ToolResult {
        call_id: call.id.clone(),
        tool_name: call.name.clone(),
        content: output.content,
        is_error: output.is_error,
    }
}

fn user_message(text: String) -> Message {
    Message::User {
        parts: vec![UserPart::Text(text)],
    }
}

fn assistant_message(text: &str, calls: &[ToolCall]) -> Message {
    let mut parts = Vec::with_capacity(calls.len() + 1);
    if !text.is_empty() {
        parts.push(AssistantPart::Text(text.to_owned()));
    }
    parts.extend(calls.iter().cloned().map(AssistantPart::ToolCall));
    Message::Assistant { parts }
}

/// Replaces the oldest tool outputs longer than `COMPACT_MIN_CHARS`, oldest first, until the
/// conversation fits `limit`. The prompt and the last `COMPACT_KEEP_TAIL` messages are never
/// touched, and only `ToolResult.content` changes, so call/result pairing holds. One pass:
/// when nothing is left to replace it stops, even above the limit.
fn compact(messages: &mut [Message], limit: usize) {
    let mut total = messages_char_len(messages);
    if total <= limit {
        return;
    }
    let protected_from = messages.len().saturating_sub(COMPACT_KEEP_TAIL);
    let Some(candidates) = messages.get_mut(1..protected_from) else {
        return;
    };
    let omitted_len = OMITTED_TOOL_OUTPUT.chars().count();
    for message in candidates {
        let Message::Tool { results } = message else {
            continue;
        };
        for result in results {
            if total <= limit {
                return;
            }
            let len = result.content.chars().count();
            if len > COMPACT_MIN_CHARS {
                total = total.saturating_sub(len) + omitted_len;
                OMITTED_TOOL_OUTPUT.clone_into(&mut result.content);
            }
        }
    }
}

/// Keeps the prompt, the latest assistant message and everything after it (its tool results
/// and any trailing user message), with tool outputs cut to `OVERFLOW_TOOL_CHARS`.
fn compact_for_overflow(messages: &mut Vec<Message>) {
    let latest = messages
        .iter()
        .rposition(|m| matches!(m, Message::Assistant { .. }));
    if let Some(latest) = latest.filter(|&i| i > 1) {
        messages.drain(1..latest);
    }
    for message in messages.iter_mut().skip(1) {
        if let Message::Tool { results } = message {
            for result in results {
                truncate_chars(&mut result.content, OVERFLOW_TOOL_CHARS);
            }
        }
    }
}

fn truncate_chars(text: &mut String, max_chars: usize) {
    if let Some((cut, _)) = text.char_indices().nth(max_chars) {
        text.truncate(cut);
        text.push_str(TRUNCATED_MARKER);
    }
}

/// Characters in the conversation (text, tool-call names and arguments, tool outputs).
fn messages_char_len(messages: &[Message]) -> usize {
    messages.iter().map(message_char_len).sum()
}

fn message_char_len(message: &Message) -> usize {
    match message {
        Message::User { parts } => parts
            .iter()
            .map(|UserPart::Text(text)| text.chars().count())
            .sum::<usize>(),
        Message::Assistant { parts } => parts
            .iter()
            .map(|part| match part {
                AssistantPart::Text(text) => text.chars().count(),
                // `provider_meta` is replayed to the provider too (Anthropic thinking blocks,
                // Gemini thought signatures), so it counts toward the context size.
                AssistantPart::ToolCall(call) => {
                    call.name.chars().count()
                        + call.arguments.to_string().chars().count()
                        + call
                            .provider_meta
                            .as_ref()
                            .map_or(0, |meta| meta.chars().count())
                }
            })
            .sum::<usize>(),
        Message::Tool { results } => results
            .iter()
            .map(|result| result.content.chars().count())
            .sum::<usize>(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::fs;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use adus_llm::{
        AssistantPart, ChatRequest, ChatResponse, ReasoningEffort, StopReason, ToolCall,
        ToolResult, UserPart,
    };
    use async_trait::async_trait;
    use serde_json::json;
    use tempfile::TempDir;

    use super::*;
    use crate::tools::ToolCache;
    use crate::workspace::RepoWorkspace;

    const SUBMIT: &str = "submit_answer";
    const NUDGE: &str =
        "Please submit your result by calling submit_answer (or return the JSON directly).";
    const WRAP_UP: &str = "You are near the end of your step or time budget. Submit your final result now using submit_answer.";
    const READ_ONLY: [&str; 4] = ["read_file", "grep", "list_dir", "find_files"];

    /// Toy terminal tool: `{"answer": "<text>"}`.
    struct SubmitAnswer {
        def: ToolDef,
    }

    impl SubmitAnswer {
        fn new() -> Self {
            Self {
                def: ToolDef {
                    name: SUBMIT.into(),
                    description: "Submit the answer.".into(),
                    parameters: json!({
                        "type": "object",
                        "properties": {"answer": {"type": "string"}},
                        "required": ["answer"]
                    }),
                },
            }
        }
    }

    impl SubmitSpec for SubmitAnswer {
        type Output = String;

        fn tool_def(&self) -> &ToolDef {
            &self.def
        }

        fn parse_arguments(&self, arguments: &Value) -> Result<String, String> {
            arguments
                .get("answer")
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| "missing string field `answer`".to_string())
        }

        fn parse_prose(&self, text: &str) -> Result<String, String> {
            let start = text.find('{').ok_or("no JSON object")?;
            let end = text.rfind('}').ok_or("no JSON object")?;
            let value: Value = serde_json::from_str(text.get(start..=end).ok_or("no JSON object")?)
                .map_err(|e| e.to_string())?;
            self.parse_arguments(&value)
        }
    }

    /// Scripted `ChatService` that records every request.
    #[derive(Default)]
    struct ScriptedChat {
        script: Mutex<VecDeque<Result<ChatResponse, ChatError>>>,
        /// How long each call takes, in call order (on the paused test clock); none = instant.
        latencies: Mutex<VecDeque<Duration>>,
        /// When set, the script is ignored: every call takes this long and answers like a model
        /// that reads files while it may and submits once only the submit tool is offered.
        investigating: Option<Duration>,
        requests: Mutex<Vec<ChatRequest>>,
        calls: Mutex<Vec<(Uuid, UsageContext)>>,
    }

    impl ScriptedChat {
        fn new(script: Vec<Result<ChatResponse, ChatError>>) -> Self {
            Self {
                script: Mutex::new(script.into()),
                ..Self::default()
            }
        }

        /// Reads `src/a.rs` while `read_file` is offered, submits `"submitted"` once only the
        /// submit tool is; every call takes `latency`.
        fn investigating(latency: Duration) -> Self {
            Self {
                investigating: Some(latency),
                ..Self::default()
            }
        }

        /// Like `new`, but call `i` sleeps `latencies[i]` before it answers.
        fn with_latencies(
            script: Vec<Result<ChatResponse, ChatError>>,
            latencies: Vec<Duration>,
        ) -> Self {
            Self {
                script: Mutex::new(script.into()),
                latencies: Mutex::new(latencies.into()),
                ..Self::default()
            }
        }

        fn requests(&self) -> Vec<ChatRequest> {
            self.requests.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl ChatService for ScriptedChat {
        async fn chat(
            &self,
            provider_id: Uuid,
            req: ChatRequest,
            ctx: UsageContext,
        ) -> Result<ChatResponse, ChatError> {
            if let Some(latency) = self.investigating {
                let id = format!("i{}", self.requests.lock().unwrap().len());
                let reads = req.tools.iter().any(|t| t.name == "read_file");
                self.requests.lock().unwrap().push(req);
                tokio::time::sleep(latency).await;
                let call = if reads {
                    read(&id, "src/a.rs")
                } else {
                    submit(&id, "submitted")
                };
                return tools_turn(vec![call]);
            }
            self.requests.lock().unwrap().push(req);
            self.calls.lock().unwrap().push((provider_id, ctx));
            let latency = self.latencies.lock().unwrap().pop_front();
            if let Some(latency) = latency {
                tokio::time::sleep(latency).await;
            }
            self.script
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Err(ChatError::Invalid("script exhausted".into())))
        }
    }

    fn call(id: &str, name: &str, arguments: Value) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: name.into(),
            arguments,
            provider_meta: None,
        }
    }

    fn tools_turn(calls: Vec<ToolCall>) -> Result<ChatResponse, ChatError> {
        Ok(ChatResponse {
            text: String::new(),
            tool_calls: calls,
            stop_reason: StopReason::ToolUse,
            input_tokens: 100,
            output_tokens: 10,
            cache_read_tokens: 7,
            cache_write_tokens: 3,
        })
    }

    fn text_turn(text: &str) -> Result<ChatResponse, ChatError> {
        Ok(ChatResponse {
            text: text.into(),
            tool_calls: Vec::new(),
            stop_reason: StopReason::EndTurn,
            input_tokens: 100,
            output_tokens: 10,
            cache_read_tokens: 7,
            cache_write_tokens: 3,
        })
    }

    fn submit(id: &str, answer: &str) -> ToolCall {
        call(id, SUBMIT, json!({"answer": answer}))
    }

    fn read(id: &str, path: &str) -> ToolCall {
        call(id, "read_file", json!({"path": path}))
    }

    fn toolbox(files: &[(&str, &str)]) -> Toolbox {
        let dir = TempDir::new().unwrap();
        for (rel, body) in files {
            let path = dir.path().join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, body).unwrap();
        }
        let ws = Arc::new(RepoWorkspace::from_temp_dir(dir).unwrap());
        Toolbox::new(ws, Arc::new(ToolCache::new()))
    }

    fn default_toolbox() -> Toolbox {
        toolbox(&[
            ("src/a.rs", "fn alpha() {}\n"),
            ("src/b.rs", "fn beta() {}\n"),
        ])
    }

    async fn run(
        chat: &ScriptedChat,
        tools: AgentTools<'_>,
        config: &AgentConfig,
    ) -> Result<AgentOutcome<String>, AgentError> {
        let usage = UsageContext {
            review_id: Some(Uuid::from_u128(7)),
            repository_id: Some(Uuid::from_u128(8)),
        };
        let llm = AgentLlm {
            chat,
            provider_id: Uuid::from_u128(1),
            model: "m1",
            system: "sys",
            effort: Some(ReasoningEffort::Medium),
            usage: &usage,
        };
        run_react_loop(&llm, tools, &SubmitAnswer::new(), config, "PROMPT".into()).await
    }

    fn tool_names(req: &ChatRequest) -> Vec<String> {
        req.tools.iter().map(|t| t.name.clone()).collect()
    }

    fn user_text(msg: &Message) -> Option<&str> {
        match msg {
            Message::User { parts } => parts.first().map(|UserPart::Text(t)| t.as_str()),
            _ => None,
        }
    }

    fn tool_results(msg: &Message) -> &[ToolResult] {
        match msg {
            Message::Tool { results } => results,
            other => panic!("expected a tool message, got {other:?}"),
        }
    }

    fn user(text: &str) -> Message {
        Message::User {
            parts: vec![UserPart::Text(text.into())],
        }
    }

    fn assistant_calls(ids: &[&str]) -> Message {
        Message::Assistant {
            parts: ids
                .iter()
                .map(|id| AssistantPart::ToolCall(read(id, "src/a.rs")))
                .collect(),
        }
    }

    fn tool_msg(ids: &[&str], content: &str) -> Message {
        Message::Tool {
            results: ids
                .iter()
                .map(|id| ToolResult {
                    call_id: (*id).into(),
                    tool_name: "read_file".into(),
                    content: content.into(),
                    is_error: false,
                })
                .collect(),
        }
    }

    /// Every assistant message with tool calls is followed by one result per call, in order.
    fn assert_paired(messages: &[Message]) {
        for (i, msg) in messages.iter().enumerate() {
            let Message::Assistant { parts } = msg else {
                continue;
            };
            let ids: Vec<&str> = parts
                .iter()
                .filter_map(|p| match p {
                    AssistantPart::ToolCall(c) => Some(c.id.as_str()),
                    AssistantPart::Text(_) => None,
                })
                .collect();
            if ids.is_empty() {
                continue;
            }
            let next = messages.get(i + 1).expect("tool message after tool calls");
            let got: Vec<&str> = tool_results(next)
                .iter()
                .map(|r| r.call_id.as_str())
                .collect();
            assert_eq!(got, ids, "results must pair with calls in order");
        }
    }

    #[tokio::test]
    async fn completes_multi_turn_tool_calls_then_submits() {
        let tb = default_toolbox();
        let chat = ScriptedChat::new(vec![
            tools_turn(vec![read("c1", "src/a.rs")]),
            tools_turn(vec![submit("c2", "done")]),
        ]);
        let out = run(&chat, AgentTools::Full(&tb), &AgentConfig::new(20))
            .await
            .unwrap();
        assert_eq!(out.result, "done");
        assert_eq!(out.steps_used, 2);
        assert!(!out.cut_short);
        assert_eq!((out.cache_read_tokens, out.cache_write_tokens), (14, 6));

        let reqs = chat.requests();
        assert_eq!(reqs.len(), 2);
        let mut expected: Vec<&str> = READ_ONLY.to_vec();
        expected.push(SUBMIT);
        assert_eq!(tool_names(&reqs[0]), expected);
        assert_eq!(reqs[0].model, "m1");
        assert_eq!(reqs[0].system, "sys");
        assert_eq!(reqs[0].max_tokens, DEFAULT_MAX_OUTPUT_TOKENS);
        assert_eq!(reqs[0].effort, Some(ReasoningEffort::Medium));
        assert_eq!(reqs[0].messages, vec![user("PROMPT")]);

        let second = &reqs[1].messages;
        assert_eq!(second.len(), 3);
        assert_paired(second);
        let results = tool_results(&second[2]);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].call_id, "c1");
        assert_eq!(results[0].tool_name, "read_file");
        assert!(!results[0].is_error);
        assert!(results[0].content.contains("fn alpha() {}"));

        let calls = chat.calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 2);
        for (provider, ctx) in calls {
            assert_eq!(provider, Uuid::from_u128(1));
            assert_eq!(ctx.review_id, Some(Uuid::from_u128(7)));
            assert_eq!(ctx.repository_id, Some(Uuid::from_u128(8)));
        }
    }

    #[tokio::test]
    async fn runs_turn_tool_calls_and_keeps_their_order() {
        let tb = default_toolbox();
        let chat = ScriptedChat::new(vec![
            tools_turn(vec![
                read("c1", "src/b.rs"),
                read("c2", "missing.rs"),
                read("c3", "src/a.rs"),
            ]),
            tools_turn(vec![submit("c4", "ok")]),
        ]);
        let mut config = AgentConfig::new(20);
        config.max_concurrent_tools = 2;
        run(&chat, AgentTools::Full(&tb), &config).await.unwrap();

        let reqs = chat.requests();
        let results = tool_results(&reqs[1].messages[2]);
        let ids: Vec<&str> = results.iter().map(|r| r.call_id.as_str()).collect();
        assert_eq!(ids, ["c1", "c2", "c3"]);
        assert!(results[0].content.contains("fn beta() {}"));
        assert!(results[1].is_error);
        assert!(results[1].content.starts_with("Error: "));
        assert!(results[2].content.contains("fn alpha() {}"));
    }

    #[tokio::test]
    async fn retries_when_submit_arguments_malformed() {
        let tb = default_toolbox();
        let chat = ScriptedChat::new(vec![
            tools_turn(vec![call("c1", SUBMIT, json!({"wrong": 1}))]),
            tools_turn(vec![submit("c2", "fixed")]),
        ]);
        let out = run(&chat, AgentTools::Full(&tb), &AgentConfig::new(6))
            .await
            .unwrap();
        assert_eq!(out.result, "fixed");
        assert_eq!(out.steps_used, 2);

        let reqs = chat.requests();
        let results = tool_results(&reqs[1].messages[2]);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].call_id, "c1");
        assert_eq!(results[0].tool_name, SUBMIT);
        assert!(results[0].is_error);
        assert!(results[0].content.starts_with("Error: "));
        assert_eq!(
            results[0].content,
            "Error: invalid submit_answer arguments: missing string field `answer`. Call submit_answer again with corrected arguments."
        );
    }

    #[tokio::test]
    async fn malformed_submit_in_multi_call_turn_answers_every_call_in_order() {
        let tb = default_toolbox();
        let chat = ScriptedChat::new(vec![
            tools_turn(vec![
                read("c1", "src/a.rs"),
                call("c2", SUBMIT, json!("not an object")),
                call("c3", "grep", json!({"pattern": "beta"})),
            ]),
            tools_turn(vec![submit("c4", "ok")]),
        ]);
        run(&chat, AgentTools::Full(&tb), &AgentConfig::new(20))
            .await
            .unwrap();

        let reqs = chat.requests();
        assert_paired(&reqs[1].messages);
        let results = tool_results(&reqs[1].messages[2]);
        let ids: Vec<&str> = results.iter().map(|r| r.call_id.as_str()).collect();
        assert_eq!(ids, ["c1", "c2", "c3"]);
        assert!(!results[0].is_error);
        assert!(results[1].is_error);
        assert_eq!(results[1].tool_name, SUBMIT);
        assert!(!results[2].is_error);
        assert!(results[2].content.contains("beta"));
    }

    #[tokio::test]
    async fn valid_submit_returns_without_running_other_calls() {
        let tb = default_toolbox();
        let chat = ScriptedChat::new(vec![tools_turn(vec![
            read("c1", "src/a.rs"),
            call("c2", SUBMIT, json!({"bad": true})),
            submit("c3", "first valid"),
        ])]);
        let out = run(&chat, AgentTools::Full(&tb), &AgentConfig::new(20))
            .await
            .unwrap();
        assert_eq!(out.result, "first valid");
        assert_eq!(out.steps_used, 1);
        assert_eq!(chat.requests().len(), 1);
    }

    #[tokio::test]
    async fn wraps_up_on_final_step_with_submit_tool_only() {
        let tb = default_toolbox();
        let chat = ScriptedChat::new(vec![
            tools_turn(vec![read("c1", "src/a.rs")]),
            tools_turn(vec![read("c2", "src/b.rs")]),
            tools_turn(vec![submit("c3", "wrapped")]),
        ]);
        let out = run(&chat, AgentTools::Full(&tb), &AgentConfig::new(3))
            .await
            .unwrap();
        assert_eq!(out.result, "wrapped");
        assert_eq!(out.steps_used, 3);
        assert!(!out.cut_short);

        let reqs = chat.requests();
        assert_eq!(reqs.len(), 3);
        assert_eq!(tool_names(&reqs[0]).len(), 5);
        assert_eq!(tool_names(&reqs[1]).len(), 5);
        assert_eq!(tool_names(&reqs[2]), [SUBMIT]);
        let last = reqs[2].messages.last().unwrap();
        assert_eq!(user_text(last), Some(WRAP_UP));
        // The reminder appears exactly once, and only on the final step.
        for req in &reqs[..2] {
            assert!(req.messages.iter().all(|m| user_text(m) != Some(WRAP_UP)));
        }
        assert_paired(&reqs[2].messages);
    }

    #[tokio::test]
    async fn wrap_up_happens_exactly_on_last_step_for_each_budget() {
        for max_steps in [1usize, 2, 6, 20] {
            let tb = default_toolbox();
            let script = (0..max_steps)
                .map(|i| tools_turn(vec![read(&format!("c{i}"), "src/a.rs")]))
                .collect();
            let chat = ScriptedChat::new(script);
            let err = run(&chat, AgentTools::Full(&tb), &AgentConfig::new(max_steps))
                .await
                .unwrap_err();
            assert!(
                matches!(err, AgentError::Unparseable(_)),
                "{max_steps}: {err:?}"
            );

            let reqs = chat.requests();
            assert_eq!(reqs.len(), max_steps, "one request per step");
            for (step, req) in reqs.iter().enumerate() {
                let final_step = step + 1 == max_steps;
                let reminders = req
                    .messages
                    .iter()
                    .filter(|m| user_text(m) == Some(WRAP_UP))
                    .count();
                assert_eq!(reminders, usize::from(final_step), "{max_steps}/{step}");
                let expected = if final_step { 1 } else { 5 };
                assert_eq!(tool_names(req).len(), expected, "{max_steps}/{step}");
                assert_paired(&req.messages);
            }
        }
    }

    #[tokio::test]
    async fn exhausted_budget_recovers_prose_as_cut_short() {
        let tb = default_toolbox();
        let mut last = tools_turn(vec![read("c1", "src/a.rs")]).unwrap();
        last.text = r#"Out of steps, here it is: {"answer": "partial"}"#.into();
        let chat = ScriptedChat::new(vec![tools_turn(vec![read("c0", "src/a.rs")]), Ok(last)]);
        let out = run(&chat, AgentTools::Full(&tb), &AgentConfig::new(2))
            .await
            .unwrap();
        assert_eq!(out.result, "partial");
        assert!(out.cut_short);
        assert_eq!(out.steps_used, 2);
    }

    #[tokio::test]
    async fn exhausted_budget_without_prose_is_unparseable() {
        let tb = default_toolbox();
        let chat = ScriptedChat::new(vec![
            tools_turn(vec![call("c0", SUBMIT, json!({}))]),
            tools_turn(vec![call("c1", SUBMIT, json!({}))]),
        ]);
        let err = run(&chat, AgentTools::Full(&tb), &AgentConfig::new(2))
            .await
            .unwrap_err();
        let AgentError::Unparseable(message) = err else {
            panic!("expected Unparseable, got {err:?}");
        };
        assert!(!message.contains("PROMPT"));
    }

    #[tokio::test]
    async fn falls_back_to_prose_json_when_model_skips_submit_tool() {
        let tb = default_toolbox();
        let chat = ScriptedChat::new(vec![text_turn(
            "Here is my answer:\n```json\n{\"answer\": \"prose\"}\n```",
        )]);
        let out = run(&chat, AgentTools::Full(&tb), &AgentConfig::new(20))
            .await
            .unwrap();
        assert_eq!(out.result, "prose");
        assert_eq!(out.steps_used, 1);
        assert!(!out.cut_short);
    }

    #[tokio::test]
    async fn nudges_once_then_accepts_submit() {
        let tb = default_toolbox();
        let chat = ScriptedChat::new(vec![
            text_turn("I think the code is fine."),
            tools_turn(vec![submit("c1", "after nudge")]),
        ]);
        let out = run(&chat, AgentTools::Full(&tb), &AgentConfig::new(20))
            .await
            .unwrap();
        assert_eq!(out.result, "after nudge");
        assert_eq!(out.steps_used, 2);

        let reqs = chat.requests();
        assert_eq!(user_text(reqs[1].messages.last().unwrap()), Some(NUDGE));
        assert_eq!(reqs[1].messages.len(), 3);
    }

    #[tokio::test]
    async fn nudges_only_once_per_run() {
        let tb = default_toolbox();
        let chat = ScriptedChat::new(vec![
            text_turn("no json here"),
            tools_turn(vec![read("c1", "src/a.rs")]),
            text_turn("still no json"),
            tools_turn(vec![submit("c2", "never reached")]),
        ]);
        let err = run(&chat, AgentTools::Full(&tb), &AgentConfig::new(20))
            .await
            .unwrap_err();
        assert!(matches!(err, AgentError::Unparseable(_)));
        let reqs = chat.requests();
        assert_eq!(reqs.len(), 3);
        let nudges = reqs[2]
            .messages
            .iter()
            .filter(|m| user_text(m) == Some(NUDGE))
            .count();
        assert_eq!(nudges, 1);
    }

    #[tokio::test]
    async fn no_nudge_when_no_steps_remain() {
        let tb = default_toolbox();
        let chat = ScriptedChat::new(vec![text_turn("no json"), text_turn("unused")]);
        let err = run(&chat, AgentTools::Full(&tb), &AgentConfig::new(1))
            .await
            .unwrap_err();
        assert!(matches!(err, AgentError::Unparseable(_)));
        assert_eq!(chat.requests().len(), 1);
    }

    #[tokio::test]
    async fn nudge_on_second_to_last_step_skips_wrap_up_reminder() {
        let tb = default_toolbox();
        let chat = ScriptedChat::new(vec![
            text_turn("no json"),
            tools_turn(vec![submit("c1", "ok")]),
        ]);
        run(&chat, AgentTools::Full(&tb), &AgentConfig::new(2))
            .await
            .unwrap();
        let reqs = chat.requests();
        assert_eq!(tool_names(&reqs[1]), [SUBMIT]);
        assert_eq!(user_text(reqs[1].messages.last().unwrap()), Some(NUDGE));
        assert!(
            reqs[1]
                .messages
                .iter()
                .all(|m| user_text(m) != Some(WRAP_UP))
        );
    }

    #[tokio::test]
    async fn no_tools_mode_sends_no_tools_and_recovers_prose_after_nudge() {
        let chat = ScriptedChat::new(vec![
            text_turn("thinking out loud"),
            text_turn(r#"{"answer": "from prose"}"#),
        ]);
        let out = run(&chat, AgentTools::NoTools, &AgentConfig::new(2))
            .await
            .unwrap();
        assert_eq!(out.result, "from prose");
        assert!(!out.cut_short);
        let reqs = chat.requests();
        assert_eq!(reqs.len(), 2);
        assert!(reqs.iter().all(|r| r.tools.is_empty()));
        assert_eq!(user_text(reqs[1].messages.last().unwrap()), Some(NUDGE));
        assert!(
            reqs[1]
                .messages
                .iter()
                .all(|m| user_text(m) != Some(WRAP_UP))
        );
    }

    #[tokio::test]
    async fn submit_only_mode_offers_only_submit_and_refuses_read_only_calls() {
        let chat = ScriptedChat::new(vec![
            tools_turn(vec![read("c1", "src/a.rs")]),
            tools_turn(vec![submit("c2", "ok")]),
        ]);
        let out = run(&chat, AgentTools::SubmitOnly, &AgentConfig::new(6))
            .await
            .unwrap();
        assert_eq!(out.result, "ok");
        let reqs = chat.requests();
        assert!(reqs.iter().all(|r| tool_names(r) == [SUBMIT]));
        let results = tool_results(&reqs[1].messages[2]);
        assert!(results[0].is_error);
        assert_eq!(
            results[0].content,
            "Error: tool read_file is not available now; call submit_answer with your result"
        );
    }

    #[tokio::test]
    async fn compacts_oldest_tool_outputs_when_exceeding_char_limit() {
        let big = "x".repeat(2_000);
        let tb = toolbox(&[("src/big.rs", &big)]);
        let chat = ScriptedChat::new(vec![
            tools_turn(vec![read("c1", "src/big.rs")]),
            tools_turn(vec![read("c2", "src/big.rs")]),
            tools_turn(vec![read("c3", "src/big.rs")]),
            tools_turn(vec![submit("c4", "ok")]),
        ]);
        let mut config = AgentConfig::new(20);
        config.compaction_char_limit = 3_000;
        run(&chat, AgentTools::Full(&tb), &config).await.unwrap();

        let reqs = chat.requests();
        let msgs = &reqs[3].messages;
        assert_eq!(msgs.len(), 7);
        assert_eq!(msgs[0], user("PROMPT"));
        assert_paired(msgs);
        // The oldest output is omitted; the last 4 messages (2 turns) stay whole.
        assert_eq!(tool_results(&msgs[2])[0].content, OMITTED_TOOL_OUTPUT);
        assert!(tool_results(&msgs[4])[0].content.contains(&big));
        assert!(tool_results(&msgs[6])[0].content.contains(&big));
    }

    #[test]
    fn compaction_replaces_oldest_first_and_stops_when_under_limit() {
        let long = "y".repeat(1_000);
        let mut msgs = vec![
            user(&"p".repeat(500)),
            assistant_calls(&["a"]),
            tool_msg(&["a"], &long),
            assistant_calls(&["b"]),
            tool_msg(&["b"], &long),
            assistant_calls(&["c"]),
            tool_msg(&["c"], &long),
            assistant_calls(&["d"]),
            tool_msg(&["d"], &long),
        ];
        let before = msgs.clone();
        // Over the limit by less than one output: only the oldest is replaced.
        compact(&mut msgs, messages_char_len(&before) - 100);
        assert_eq!(tool_results(&msgs[2])[0].content, OMITTED_TOOL_OUTPUT);
        assert_eq!(msgs[3..], before[3..]);
        assert_eq!(msgs[0], before[0]);
    }

    #[test]
    fn compaction_terminates_when_nothing_is_left_to_compact() {
        let long = "z".repeat(1_000);
        let mut msgs = vec![
            user(&long),
            assistant_calls(&["a"]),
            tool_msg(&["a"], "short output"),
            assistant_calls(&["b"]),
            tool_msg(&["b"], &long),
            assistant_calls(&["c"]),
            tool_msg(&["c"], &long),
        ];
        let before = msgs.clone();
        compact(&mut msgs, 0);
        // The prompt, short outputs and the last 4 messages are never touched.
        assert_eq!(msgs, before);
    }

    #[test]
    fn overflow_compaction_keeps_prompt_latest_turn_and_trailing_user() {
        let long = "w".repeat(2_000);
        let mut msgs = vec![
            user("PROMPT"),
            assistant_calls(&["a"]),
            tool_msg(&["a"], &long),
            assistant_calls(&["b", "c"]),
            tool_msg(&["b", "c"], &long),
            user(WRAP_UP),
        ];
        compact_for_overflow(&mut msgs);
        assert_eq!(msgs.len(), 4);
        assert_eq!(msgs[0], user("PROMPT"));
        assert_eq!(msgs[1], assistant_calls(&["b", "c"]));
        assert_paired(&msgs);
        for result in tool_results(&msgs[2]) {
            assert!(result.content.starts_with(&"w".repeat(500)));
            assert!(!result.content.starts_with(&"w".repeat(501)));
        }
        assert_eq!(msgs[3], user(WRAP_UP));
    }

    #[tokio::test]
    async fn recovers_once_from_context_overflow_error() {
        let tb = toolbox(&[("src/big.rs", &"v".repeat(3_000))]);
        let chat = ScriptedChat::new(vec![
            tools_turn(vec![read("c1", "src/big.rs")]),
            tools_turn(vec![read("c2", "src/big.rs")]),
            Err(ChatError::ContextOverflow("too long".into())),
            tools_turn(vec![submit("c3", "recovered")]),
        ]);
        let out = run(&chat, AgentTools::Full(&tb), &AgentConfig::new(20))
            .await
            .unwrap();
        assert_eq!(out.result, "recovered");
        // The overflowed call is repeated, not counted as a new step.
        assert_eq!(out.steps_used, 3);

        let reqs = chat.requests();
        assert_eq!(reqs.len(), 4);
        assert_eq!(reqs[2].messages.len(), 5);
        let retried = &reqs[3].messages;
        assert_eq!(retried.len(), 3);
        assert_eq!(retried[0], user("PROMPT"));
        assert_eq!(retried[1], reqs[2].messages[3]);
        assert_paired(retried);
        assert!(tool_results(&retried[2])[0].content.chars().count() < 600);
        assert_eq!(tool_names(&reqs[3]), tool_names(&reqs[2]));
    }

    #[tokio::test]
    async fn second_context_overflow_is_returned() {
        let tb = default_toolbox();
        let chat = ScriptedChat::new(vec![
            tools_turn(vec![read("c1", "src/a.rs")]),
            Err(ChatError::ContextOverflow("too long".into())),
            Err(ChatError::ContextOverflow("still too long".into())),
            tools_turn(vec![submit("c2", "unused")]),
        ]);
        let err = run(&chat, AgentTools::Full(&tb), &AgentConfig::new(20))
            .await
            .unwrap_err();
        assert_eq!(
            err,
            AgentError::Chat(ChatError::ContextOverflow("still too long".into()))
        );
        assert_eq!(chat.requests().len(), 3);
    }

    #[tokio::test]
    async fn propagates_tools_unsupported_immediately() {
        let tb = default_toolbox();
        let chat = ScriptedChat::new(vec![
            Err(ChatError::ToolsUnsupported("no tools here".into())),
            tools_turn(vec![submit("c1", "unused")]),
        ]);
        let err = run(&chat, AgentTools::Full(&tb), &AgentConfig::new(20))
            .await
            .unwrap_err();
        assert_eq!(err, AgentError::ToolsUnsupported("no tools here".into()));
        assert_eq!(chat.requests().len(), 1);
    }

    #[tokio::test]
    async fn other_chat_errors_are_returned_as_chat() {
        let chat = ScriptedChat::new(vec![Err(ChatError::NoKeys)]);
        let err = run(&chat, AgentTools::SubmitOnly, &AgentConfig::new(6))
            .await
            .unwrap_err();
        assert_eq!(err, AgentError::Chat(ChatError::NoKeys));
        assert_eq!(chat.requests().len(), 1);
    }

    fn budget_note(used: usize, max: usize) -> String {
        format!(
            "Step budget: {used} of {max} steps used. Gather only the evidence you still need, then call submit_answer."
        )
    }

    /// How many user messages of `req` are exactly `text`.
    fn count_user(req: &ChatRequest, text: &str) -> usize {
        req.messages
            .iter()
            .filter(|m| user_text(m) == Some(text))
            .count()
    }

    /// Budget notes of any step count in `req`.
    fn count_notes(req: &ChatRequest) -> usize {
        req.messages
            .iter()
            .filter(|m| user_text(m).is_some_and(|t| t.starts_with("Step budget: ")))
            .count()
    }

    #[tokio::test]
    async fn wrap_up_window_offers_only_submit_for_the_last_two_steps() {
        let tb = default_toolbox();
        let mut script: Vec<_> = (1..=5)
            .map(|i| tools_turn(vec![read(&format!("c{i}"), "src/a.rs")]))
            .collect();
        script.push(tools_turn(vec![submit("c6", "wrapped")]));
        let chat = ScriptedChat::new(script);
        let mut config = AgentConfig::new(6);
        config.wrap_up_steps = 2;
        let out = run(&chat, AgentTools::Full(&tb), &config).await.unwrap();
        assert_eq!(out.result, "wrapped");
        assert_eq!(out.steps_used, 6);
        assert!(!out.cut_short);

        let reqs = chat.requests();
        assert_eq!(reqs.len(), 6);
        for (step, req) in reqs.iter().enumerate() {
            let wrap_up = step >= 4;
            let expected = if wrap_up { 1 } else { 5 };
            assert_eq!(tool_names(req).len(), expected, "step {step}");
            // One reminder, sent when the window starts, then kept in the history.
            assert_eq!(
                count_user(req, WRAP_UP),
                usize::from(wrap_up),
                "step {step}"
            );
            assert_paired(&req.messages);
        }
        assert_eq!(user_text(reqs[4].messages.last().unwrap()), Some(WRAP_UP));
        assert_eq!(tool_names(&reqs[4]), [SUBMIT]);
        // The read call made inside the window is refused, still paired with its call.
        let refused = tool_results(reqs[5].messages.last().unwrap());
        assert_eq!(refused[0].call_id, "c5");
        assert!(refused[0].is_error);
        assert_eq!(
            refused[0].content,
            "Error: tool read_file is not available now; call submit_answer with your result"
        );

        // A window larger than the budget makes every step a wrap-up step.
        let chat = ScriptedChat::new(vec![
            tools_turn(vec![read("c1", "src/a.rs")]),
            tools_turn(vec![submit("c2", "ok")]),
        ]);
        let mut config = AgentConfig::new(3);
        config.wrap_up_steps = 5;
        run(&chat, AgentTools::Full(&tb), &config).await.unwrap();
        let reqs = chat.requests();
        assert!(reqs.iter().all(|r| tool_names(r) == [SUBMIT]));
        assert_eq!(user_text(reqs[0].messages.last().unwrap()), Some(WRAP_UP));
        assert!(reqs.iter().all(|r| count_user(r, WRAP_UP) == 1));
        assert_paired(&reqs[1].messages);
    }

    #[tokio::test(start_paused = true)]
    async fn deadline_close_switches_to_submit_only_early() {
        let tb = default_toolbox();
        let mut script: Vec<_> = (1..=3)
            .map(|i| tools_turn(vec![read(&format!("c{i}"), "src/a.rs")]))
            .collect();
        script.push(tools_turn(vec![submit("c4", "in time")]));
        let secs = Duration::from_secs;
        let chat =
            ScriptedChat::with_latencies(script, vec![secs(30), secs(10), secs(10), secs(10)]);
        let deadline = Instant::now() + secs(100);
        let mut config = AgentConfig::new(20);
        config.deadline = Some(deadline);
        let out = run(&chat, AgentTools::Full(&tb), &config).await.unwrap();
        assert_eq!(out.result, "in time");
        assert_eq!(out.steps_used, 4);
        assert!(!out.cut_short);
        assert!(Instant::now() <= deadline, "submitted before the deadline");

        // Steps end at 30, 40 and 50 s. Before step 4, 50 s are left: less than 2 × the slowest
        // step (60 s), so step 4 offers only the submit tool, with one reminder.
        let reqs = chat.requests();
        assert_eq!(reqs.len(), 4);
        for req in &reqs[..3] {
            assert_eq!(tool_names(req).len(), 5);
            assert_eq!(count_user(req, WRAP_UP), 0);
        }
        assert_eq!(tool_names(&reqs[3]), [SUBMIT]);
        assert_eq!(user_text(reqs[3].messages.last().unwrap()), Some(WRAP_UP));
        assert_eq!(count_user(&reqs[3], WRAP_UP), 1);
        assert_paired(&reqs[3].messages);
    }

    /// The case the 2 × whole-step margin exists for: every step takes as long as the slowest
    /// one, and the model only submits once it is told to (it reads while it may).
    #[tokio::test(start_paused = true)]
    async fn steady_latency_still_submits_before_the_deadline() {
        let tb = default_toolbox();
        let chat = ScriptedChat::investigating(Duration::from_secs(10));
        let deadline = Instant::now() + Duration::from_secs(95);
        let mut config = AgentConfig::new(50);
        config.deadline = Some(deadline);
        // The caller's hard cut, as the Finder and the Verifier apply it.
        let run = run(&chat, AgentTools::Full(&tb), &config);
        let out = tokio::time::timeout_at(deadline, run)
            .await
            .expect("submitted before the hard cut")
            .unwrap();
        assert_eq!(out.result, "submitted");
        assert!(!out.cut_short);
        assert!(Instant::now() <= deadline);

        // Steps end every 10 s. Before step 9 (at 80 s) 15 s are left, less than 2 × 10 s, so
        // step 9 only offers the submit tool and ends at 90 s. (With 1.5 × it would have read
        // once more and submitted at 100 s, after the cut.)
        assert_eq!(out.steps_used, 9);
        let reqs = chat.requests();
        assert_eq!(reqs.len(), 9);
        assert!(reqs[..8].iter().all(|r| tool_names(r).len() == 5));
        assert_eq!(tool_names(&reqs[8]), [SUBMIT]);
        assert_eq!(count_user(&reqs[8], WRAP_UP), 1);
        assert_paired(&reqs[8].messages);
    }

    #[tokio::test(start_paused = true)]
    async fn first_step_never_triggers_time_wrap_up() {
        let tb = default_toolbox();
        let chat = ScriptedChat::with_latencies(
            vec![
                tools_turn(vec![read("c1", "src/a.rs")]),
                tools_turn(vec![submit("c2", "late")]),
            ],
            vec![Duration::from_secs(5), Duration::from_secs(5)],
        );
        // Far too close for any step, but no step has a measured latency yet.
        let mut config = AgentConfig::new(20);
        config.deadline = Some(Instant::now() + Duration::from_millis(1));
        let out = run(&chat, AgentTools::Full(&tb), &config).await.unwrap();
        assert_eq!(out.result, "late");

        let reqs = chat.requests();
        assert_eq!(tool_names(&reqs[0]).len(), 5);
        assert_eq!(reqs[0].messages, vec![user("PROMPT")]);
        // After one measured step the deadline rule applies.
        assert_eq!(tool_names(&reqs[1]), [SUBMIT]);
        assert_eq!(count_user(&reqs[1], WRAP_UP), 1);
        assert_paired(&reqs[1].messages);
    }

    #[tokio::test]
    async fn budget_note_sent_once_at_half_budget() {
        let tb = default_toolbox();
        let reads = |n: usize| -> Vec<_> {
            (1..=n)
                .map(|i| tools_turn(vec![read(&format!("c{i}"), "src/a.rs")]))
                .collect()
        };

        // Verifier-like budget: the note goes before step 4 (3 of 6 used), once.
        let mut script = reads(5);
        script.push(tools_turn(vec![submit("c6", "ok")]));
        let chat = ScriptedChat::new(script);
        run(&chat, AgentTools::Full(&tb), &AgentConfig::new(6))
            .await
            .unwrap();
        let reqs = chat.requests();
        let note = budget_note(3, 6);
        for (step, req) in reqs.iter().enumerate() {
            assert_eq!(count_notes(req), usize::from(step >= 3), "step {step}");
            assert_paired(&req.messages);
        }
        assert_eq!(
            user_text(reqs[3].messages.last().unwrap()),
            Some(note.as_str())
        );
        // The wrap-up reminder comes later, on its own step.
        assert_eq!(count_user(&reqs[3], WRAP_UP), 0);
        assert_eq!(count_user(&reqs[5], WRAP_UP), 1);

        // Finder budget: 10 of 20, before the two-step wrap-up window.
        let mut script = reads(19);
        script.push(tools_turn(vec![submit("c20", "ok")]));
        let chat = ScriptedChat::new(script);
        let mut config = AgentConfig::new(20);
        config.wrap_up_steps = 2;
        run(&chat, AgentTools::Full(&tb), &config).await.unwrap();
        let reqs = chat.requests();
        assert_eq!(
            user_text(reqs[10].messages.last().unwrap()),
            Some(budget_note(10, 20).as_str())
        );
        assert_eq!(count_notes(&reqs[9]), 0);
        assert_eq!(count_notes(&reqs[19]), 1);

        // No note when the half-way step already starts the wrap-up (the reminder wins),
        // for budgets under 4 steps, or without the repository tools.
        let mut window = AgentConfig::new(4);
        window.wrap_up_steps = 2;
        let cases = [
            (AgentTools::Full(&tb), window, 4),
            (AgentTools::Full(&tb), AgentConfig::new(3), 3),
            (AgentTools::SubmitOnly, AgentConfig::new(6), 6),
        ];
        for (tools, config, steps) in cases {
            let chat = ScriptedChat::new(reads(steps));
            let err = run(&chat, tools, &config).await.unwrap_err();
            assert!(matches!(err, AgentError::Unparseable(_)), "{err:?}");
            let reqs = chat.requests();
            assert_eq!(reqs.len(), steps);
            assert!(reqs.iter().all(|r| count_notes(r) == 0), "{steps} steps");
            assert_eq!(count_user(&reqs[steps - 1], WRAP_UP), 1);
        }
    }

    #[tokio::test]
    async fn exhausted_budget_switches_agent_to_submit_only() {
        let tb = default_toolbox();
        // Every turn reports 100 input tokens; the ceiling is crossed by the third step.
        let mut script: Vec<_> = (1..=3)
            .map(|i| tools_turn(vec![read(&format!("c{i}"), "src/a.rs")]))
            .collect();
        script.push(tools_turn(vec![submit("c4", "within budget")]));
        let chat = ScriptedChat::new(script);
        let budget = Arc::new(TokenBudget::new(250));
        let mut config = AgentConfig::new(20);
        config.token_budget = Some(Arc::clone(&budget));
        let out = run(&chat, AgentTools::Full(&tb), &config).await.unwrap();
        assert_eq!(out.result, "within budget");
        assert_eq!(out.steps_used, 4);
        assert_eq!(budget.used(), 400, "every response is counted once");

        let reqs = chat.requests();
        for req in &reqs[..3] {
            assert_eq!(tool_names(req).len(), 5);
            assert_eq!(count_user(req, WRAP_UP), 0);
        }
        assert_eq!(tool_names(&reqs[3]), [SUBMIT]);
        assert_eq!(user_text(reqs[3].messages.last().unwrap()), Some(WRAP_UP));
        assert_eq!(count_user(&reqs[3], WRAP_UP), 1);
        assert_paired(&reqs[3].messages);

        // A budget another agent of the review already used up: this agent still takes one
        // investigating step, then only submits.
        let chat = ScriptedChat::new(vec![
            tools_turn(vec![read("c1", "src/a.rs")]),
            tools_turn(vec![submit("c2", "late")]),
        ]);
        let out = run(&chat, AgentTools::Full(&tb), &config).await.unwrap();
        assert_eq!(out.result, "late");
        let reqs = chat.requests();
        assert_eq!(tool_names(&reqs[0]).len(), 5);
        assert_eq!(tool_names(&reqs[1]), [SUBMIT]);
        assert_eq!(count_user(&reqs[1], WRAP_UP), 1);

        // A failed call adds nothing.
        let chat = ScriptedChat::new(vec![Err(ChatError::NoKeys)]);
        let fresh = Arc::new(TokenBudget::new(250));
        let mut config = AgentConfig::new(20);
        config.token_budget = Some(Arc::clone(&fresh));
        run(&chat, AgentTools::Full(&tb), &config)
            .await
            .unwrap_err();
        assert_eq!(fresh.used(), 0);
    }

    #[test]
    fn run_future_is_send() {
        fn assert_send<F: Send>(_: F) {}
        let chat = ScriptedChat::new(Vec::new());
        let usage = UsageContext::default();
        let llm = AgentLlm {
            chat: &chat,
            provider_id: Uuid::nil(),
            model: "m",
            system: "s",
            effort: None,
            usage: &usage,
        };
        let tb = default_toolbox();
        let submit = SubmitAnswer::new();
        let config = AgentConfig::new(1);
        assert_send(run_react_loop(
            &llm,
            AgentTools::Full(&tb),
            &submit,
            &config,
            String::new(),
        ));
    }
}
