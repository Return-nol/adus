//! Reusable ReAct agent loop: the model calls read-only repository tools until it submits its
//! result through one terminal tool. Context compaction and a one-shot context-overflow
//! recovery keep long runs within the model's window. Used by the Finder and the Verifier.
//!
//! Loop policies, each switched on through [`AgentConfig`]: coverage debt notes and the one-shot
//! completion gate (gated Finder runs), step-budget band notes (Finder runs), a tool-free final
//! step with a verdict-due note, and widening of short `read_file` calls (Verifier runs). Every
//! note is appended to the conversation and kept there: the history is append-only, so the
//! prompt-cache prefix stays valid and providers that check preserved thinking accept it.

use std::borrow::Cow;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use adus_llm::router::{ChatError, ChatService, UsageContext};
use adus_llm::{
    AssistantPart, ChatRequest, ChatResponse, Message, ReasoningEffort, ToolCall, ToolDef,
    ToolResult, UserPart,
};
use futures::{StreamExt, stream};
use serde_json::{Value, json};
use tokio::time::Instant;
use uuid::Uuid;

use crate::budget::TokenBudget;
use crate::coverage::{CoverageLedger, PendingSignature};
use crate::prompt::truncate_with_marker;
use crate::redact::redact_secrets;
use crate::tools::{ToolOutput, Toolbox, parse_read_header_range};

/// Target of the live agent trace, enabled with `RUST_LOG=...,adus_review::trace=trace`: each
/// run's prompt, and per step the model's reasoning, reply text, tool calls and tool results
/// (secrets redacted).
const TRACE_TARGET: &str = "adus_review::trace";
/// Most characters of one tool result shown in the trace.
const TRACE_RESULT_CHARS: usize = 2_000;

/// Default `AgentConfig::compaction_char_limit`.
const DEFAULT_COMPACTION_CHAR_LIMIT: usize = 320_000;
const MAX_OUTPUT_TOKENS: u32 = 8_192;
/// Default `AgentConfig::max_concurrent_tools`.
const DEFAULT_MAX_CONCURRENT_TOOLS: usize = 4;
/// Default `AgentConfig::wrap_up_steps`: only the final step is a wrap-up step.
const DEFAULT_WRAP_UP_STEPS: usize = 1;

/// Prefix length preserved when shortening older tool outputs during compaction.
const COMPACT_EXCERPT_CHARS: usize = 400;
/// Appended to a shortened tool output during compaction.
const COMPACTED_MARKER: &str = "\n[older tool output shortened to save context]";
/// The newest messages (two turns) are never compacted.
const COMPACT_KEEP_TAIL: usize = 4;
/// A run with a `deadline` wraps up once the time left is less than this many times its
/// slowest whole step (LLM call plus tools). With 2, at least one such step is left when
/// wrap-up starts, enough for the submit step.
const WRAP_UP_STEP_FACTOR: u32 = 2;
/// Budget band notes are only sent to runs with at least this many steps.
const BUDGET_BANDS_MIN_STEPS: usize = 6;
/// The completion gate only refuses while at least this many investigating steps remain after
/// the current one, so the model can still read what it skipped.
pub(crate) const GATE_MIN_STEPS_LEFT: usize = 2;
/// `read_file` calls spanning fewer lines than this are widened (`widen_short_reads`).
const VERIFY_WIDEN_SPAN_THRESHOLD: u64 = 20;
/// Lines added before a widened `read_file` window.
const VERIFY_WIDEN_BEFORE: u64 = 20;
/// Lines added after a widened `read_file` window.
const VERIFY_WIDEN_AFTER: u64 = 10;
/// Tool outputs kept after a context overflow are cut to this many chars.
const OVERFLOW_TOOL_CHARS: usize = 500;
/// Appended to a tool output cut after a context overflow.
const TRUNCATED_MARKER: &str = "\n[output truncated]";

/// Limits and policies of one agent run.
#[derive(Debug, Clone)]
pub(crate) struct AgentConfig {
    /// LLM calls allowed in one run (Finder 20 to 30, Verifier 6).
    pub max_steps: usize,
    /// Old tool outputs are compacted while the conversation is longer than this (chars).
    pub compaction_char_limit: usize,
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
    /// The changed hunks the run should read. With the repository tools and outside wrap-up,
    /// a coverage debt note is appended whenever the set of unread qualifying hunks changes
    /// (never on the first request). Only gated Finder runs carry one.
    pub coverage_ledger: Option<CoverageLedger>,
    /// Refuse the first submission once while qualifying hunks of `coverage_ledger` are unread
    /// and at least [`GATE_MIN_STEPS_LEFT`] investigating steps remain.
    pub enforce_gate: bool,
    /// The final step offers no tools and asks for the result as JSON text; one step earlier a
    /// verdict-due note says one reading step is left.
    pub no_tools_on_last_step: bool,
    /// Escalating step-budget notes (`Encourage`, then `Urgent`). Finder runs only: the texts
    /// speak of findings and dismissals.
    pub budget_bands: bool,
    /// `read_file` calls spanning fewer than [`VERIFY_WIDEN_SPAN_THRESHOLD`] lines are widened
    /// by [`VERIFY_WIDEN_BEFORE`] lines before and [`VERIFY_WIDEN_AFTER`] after.
    pub widen_short_reads: bool,
}

impl AgentConfig {
    /// `max_steps` with the default limits for everything else and every policy off.
    pub(crate) fn new(max_steps: usize) -> Self {
        Self {
            max_steps,
            compaction_char_limit: DEFAULT_COMPACTION_CHAR_LIMIT,
            max_concurrent_tools: DEFAULT_MAX_CONCURRENT_TOOLS,
            wrap_up_steps: DEFAULT_WRAP_UP_STEPS,
            deadline: None,
            token_budget: None,
            coverage_ledger: None,
            enforce_gate: false,
            no_tools_on_last_step: false,
            budget_bands: false,
            widen_short_reads: false,
        }
    }
}

/// A finished run.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AgentOutcome<T> {
    pub result: T,
    /// LLM calls made (the overflow retry repeats a step and is not counted twice).
    pub steps_used: usize,
    pub cache_read_tokens: u32,
    pub cache_write_tokens: u32,
    /// The step budget ran out and `result` was recovered from the last reply's prose, or the
    /// run ended unparseable and `result` is the submission the completion gate held.
    pub cut_short: bool,
    /// The completion gate refused a submission in this run.
    pub gate_refused: bool,
    /// Normalized paths of every file a `read_file` call of this run showed successfully.
    pub opened_files: HashSet<String>,
    /// Mean wall time of the run's LLM calls; `None` when none completed.
    pub mean_step: Option<Duration>,
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub(crate) enum AgentError {
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
pub(crate) trait SubmitSpec: Send + Sync {
    type Output: Send;

    /// Definition sent to the model (for example `submit_findings`).
    fn tool_def(&self) -> &ToolDef;

    /// Reads the terminal tool's arguments. The error text is shown to the model;
    /// `can_retry` is `true` when at least one step remains after this turn.
    fn parse_arguments(&self, arguments: &Value, can_retry: bool) -> Result<Self::Output, String>;

    /// Recovers the result from a reply that did not call the terminal tool.
    fn parse_prose(&self, text: &str) -> Result<Self::Output, String>;

    /// Whether `output` reports findings. An accepted result without any falls back to the
    /// findings of the latest earlier submission the completion gate refused. Results that
    /// carry no findings (the default) never fall back.
    fn has_findings(&self, _output: &Self::Output) -> bool {
        true
    }

    /// `accepted` with the findings of `earlier` (a refused submission that had some); every
    /// other part of `accepted` is kept. Only called when `has_findings(accepted)` is false.
    fn with_findings_of(&self, accepted: Self::Output, _earlier: Self::Output) -> Self::Output {
        accepted
    }
}

/// Which tools the model gets.
#[derive(Debug, Clone, Copy)]
pub(crate) enum AgentTools<'a> {
    /// Read-only repository tools plus the terminal tool.
    Full(&'a Toolbox),
    /// Only the terminal tool (no checkout, but the model supports tools).
    SubmitOnly,
    /// No tools at all (the model rejected tool calling); results come from prose.
    NoTools,
}

/// Where and how the agent talks to the model.
#[derive(Clone, Copy)]
pub(crate) struct AgentLlm<'a> {
    pub chat: &'a dyn ChatService,
    pub provider_id: Uuid,
    pub model: &'a str,
    pub system: &'a str,
    pub effort: Option<ReasoningEffort>,
    /// Cloned into every `chat` call.
    pub usage: &'a UsageContext,
}

/// Single-turn low-effort text call, recording `input_tokens` into `token_budget`.
pub(crate) async fn chat_low_effort(
    llm: &AgentLlm<'_>,
    token_budget: Option<&TokenBudget>,
    prompt: String,
    max_tokens: u32,
) -> Result<String, ChatError> {
    let request = ChatRequest::text(llm.model, llm.system, prompt, max_tokens)
        .with_effort(llm.effort.unwrap_or(ReasoningEffort::Low));
    let response = llm
        .chat
        .chat(llm.provider_id, request, llm.usage.clone())
        .await?;
    if let Some(budget) = token_budget {
        budget.add(response.input_tokens, response.cache_read_tokens);
    }
    Ok(response.text)
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
/// terminal tool is offered, and one reminder to submit is sent when that starts. With
/// `no_tools_on_last_step` the final step offers no tools at all and asks for the JSON result.
///
/// **Notes** (with the repository tools, outside wrap-up, appended and kept): the coverage
/// debt note when the unread hunks change, the budget band notes, and the verdict-due note.
///
/// **Completion gate** (`enforce_gate` with a `coverage_ledger`): the first parsed submission,
/// tool call or prose, is refused once while qualifying hunks are unread and enough steps
/// remain; the refused submission is held and returned (`cut_short`) if the run later ends
/// without a parsable result.
///
/// Cancel-safe: all state is local to the future, so dropping it (a deadline) ends the run.
/// Tool work already handed to `spawn_blocking` finishes on its own.
pub(crate) async fn run_react_loop<S: SubmitSpec>(
    llm: &AgentLlm<'_>,
    tools: AgentTools<'_>,
    submit: &S,
    config: &AgentConfig,
    prompt: String,
) -> Result<AgentOutcome<S::Output>, AgentError> {
    let submit_def = submit.tool_def();
    let submit_name = submit_def.name.as_str();
    let with_repo_tools = matches!(tools, AgentTools::Full(_));
    let mut messages = vec![user_message(prompt)];
    let mut tally = Tally::default();
    let mut overflow_retried = false;
    let mut nudged = false;
    let mut nudged_last_step = false;
    let mut last_text = String::new();
    let mut wrap_up = WrapUp::default();
    let mut notes = Notes::default();
    let mut ledger = config.coverage_ledger.clone().filter(|_| with_repo_tools);
    let mut gate = CompletionGate::new(config.enforce_gate && ledger.is_some());
    trace_prompt(llm.system, &messages);

    while tally.steps_used < config.max_steps {
        let starts_wrap_up = wrap_up.check(tally.steps_used, config);
        let text_only_step =
            config.no_tools_on_last_step && tally.steps_used + 1 == config.max_steps;
        if !matches!(tools, AgentTools::NoTools) {
            if text_only_step {
                messages.push(user_message(format!(
                    "This is your last step and no tools are available. Reply with only your final result as the JSON object you would pass to {submit_name}, based on what you have read."
                )));
            } else if starts_wrap_up && !nudged_last_step {
                // A nudge sent just before already asks for the result; one request is enough.
                messages.push(user_message(format!(
                    "You are near the end of your budget. Submit your final result now using {submit_name}."
                )));
            }
        }
        if with_repo_tools && !wrap_up.active() {
            notes.append(
                &mut messages,
                config,
                tally.steps_used,
                ledger.as_ref(),
                submit_name,
            );
        }
        nudged_last_step = false;
        let (read_only, offered) = match (tools, wrap_up.active()) {
            _ if text_only_step => (None, Vec::new()),
            (AgentTools::NoTools, _) => (None, Vec::new()),
            (AgentTools::Full(toolbox), false) => {
                let mut defs = toolbox.read_only_tool_defs().to_vec();
                defs.push(submit_def.clone());
                (Some(toolbox), defs)
            }
            (AgentTools::Full(_) | AgentTools::SubmitOnly, _) => (None, vec![submit_def.clone()]),
        };

        // The whole step is timed (LLM call plus tools): the slowest one sizes the margin a
        // deadline-close run keeps for its submit step.
        let step_started = Instant::now();
        compact(&mut messages, config.compaction_char_limit);
        let response = request(llm, &mut messages, offered, &mut overflow_retried).await?;
        tally.add(&response, step_started.elapsed());
        if let Some(budget) = &config.token_budget {
            budget.add(response.input_tokens, response.cache_read_tokens);
        }
        tracing::debug!(
            step = tally.steps_used,
            tools = ?response.tool_calls.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            stop_reason = ?response.stop_reason,
            wrap_up = wrap_up.reason,
            "agent step"
        );
        trace_response(tally.steps_used, &response);
        let ChatResponse {
            text,
            tool_calls,
            provider_meta,
            ..
        } = response;
        messages.push(assistant_message(&text, &tool_calls, provider_meta));
        let pending = gate.pending(config, wrap_up.active(), tally.steps_used, ledger.as_ref());

        if tool_calls.is_empty() {
            if let Ok(result) = submit.parse_prose(&text) {
                match pending {
                    Some(files) => {
                        messages.push(user_message(gate_refusal(&files, submit_name)));
                        gate.hold(result);
                        wrap_up.record(step_started.elapsed());
                        continue;
                    }
                    None => return Ok(gate.accept(submit, result, &mut tally)),
                }
            }
            if nudged || tally.steps_used >= config.max_steps {
                return gate.give_up(submit_name, &mut tally);
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
        // A submit the gate refuses is held, and any later submit of that turn is refused too.
        let can_retry = tally.steps_used < config.max_steps;
        let mut plans = Vec::with_capacity(tool_calls.len());
        let mut refusal: Option<String> = None;
        for call in &tool_calls {
            if call.name != submit_name {
                plans.push(CallPlan::Run);
                continue;
            }
            if let Some(text) = &refusal {
                plans.push(CallPlan::Refused(text.clone()));
                continue;
            }
            match submit.parse_arguments(&call.arguments, can_retry) {
                Ok(result) => match &pending {
                    Some(files) => {
                        let text = gate_refusal(files, submit_name);
                        gate.hold(result);
                        plans.push(CallPlan::Refused(text.clone()));
                        refusal = Some(text);
                    }
                    None => return Ok(gate.accept(submit, result, &mut tally)),
                },
                Err(err) => plans.push(CallPlan::Invalid(err)),
            }
        }
        let executed: Cow<'_, [ToolCall]> = if config.widen_short_reads {
            Cow::Owned(
                tool_calls
                    .iter()
                    .map(|call| widen_short_read(call).unwrap_or_else(|| call.clone()))
                    .collect(),
            )
        } else {
            Cow::Borrowed(&tool_calls)
        };
        let results = run_tool_calls(
            &executed,
            plans,
            read_only,
            submit_name,
            config.max_concurrent_tools,
        )
        .await;
        if let Some(toolbox) = read_only {
            record_reads(
                toolbox,
                &executed,
                &results,
                ledger.as_mut(),
                &mut tally.opened_files,
            )
            .await;
        }
        trace_results(tally.steps_used, &results);
        messages.push(Message::Tool { results });
        last_text = text;
        wrap_up.record(step_started.elapsed());
    }

    match submit.parse_prose(&last_text) {
        Ok(result) => {
            let mut outcome = gate.accept(submit, result, &mut tally);
            outcome.cut_short = true;
            Ok(outcome)
        }
        Err(_) => gate.give_up(submit_name, &mut tally),
    }
}

/// The gate's one refusal: which files to read and that the next submit is accepted.
fn gate_refusal(files: &[String], submit_name: &str) -> String {
    format!(
        "Not submitted yet. Before you submit, open these changed files with read_file and check the code around each change: {}. Then call {submit_name} again with every finding you confirmed. This is asked only once; your next {submit_name} call will be accepted.",
        files.join(", ")
    )
}

/// A `read_file` call spanning fewer than [`VERIFY_WIDEN_SPAN_THRESHOLD`] lines, widened by
/// [`VERIFY_WIDEN_BEFORE`] / [`VERIFY_WIDEN_AFTER`] lines; `None` for any other call. The
/// tool's own line and character caps still apply to the widened window.
fn widen_short_read(call: &ToolCall) -> Option<ToolCall> {
    if call.name != "read_file" {
        return None;
    }
    let start = call.arguments.get("start_line")?.as_u64()?;
    let end = call.arguments.get("end_line")?.as_u64()?;
    if end < start || end - start >= VERIFY_WIDEN_SPAN_THRESHOLD {
        return None;
    }
    let mut widened = call.clone();
    let args = widened.arguments.as_object_mut()?;
    args.insert(
        "start_line".into(),
        json!(start.saturating_sub(VERIFY_WIDEN_BEFORE).max(1)),
    );
    args.insert(
        "end_line".into(),
        json!(end.saturating_add(VERIFY_WIDEN_AFTER)),
    );
    Some(widened)
}

/// Records every successful `read_file` of a turn whose output has a `[path: lines X-Y of Z]`
/// header: the lines shown go into `ledger`, and its file into `opened`. Path resolution runs off the async runtime.
async fn record_reads(
    toolbox: &Toolbox,
    calls: &[ToolCall],
    results: &[ToolResult],
    mut ledger: Option<&mut CoverageLedger>,
    opened: &mut HashSet<String>,
) {
    for (call, result) in calls.iter().zip(results) {
        if result.is_error {
            continue;
        }
        // Only the lines the output header says were shown count; without a header nothing
        // is credited.
        let Some((start, end)) = parse_read_header_range(&result.content) else {
            continue;
        };
        let Some((path, _, _)) = toolbox.read_file_range(call).await else {
            continue;
        };
        if let Some(ledger) = ledger.as_deref_mut() {
            ledger.record_read(&path, start, end);
        }
        opened.insert(path);
    }
}

/// How far into its step budget a Finder run is; each band's note is sent once on entry.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
enum BudgetBand {
    #[default]
    Free,
    Encourage,
    Urgent,
}

impl BudgetBand {
    /// The band after `steps_used` steps. `Urgent` starts three steps before wrap-up (but not
    /// before step 3), `Encourage` four steps earlier (but not before step 2).
    fn at(steps_used: usize, config: &AgentConfig) -> Self {
        let urgent_from = force_start(config).saturating_sub(3).max(3);
        let encourage_from = urgent_from.saturating_sub(4).max(2);
        if steps_used >= urgent_from {
            Self::Urgent
        } else if steps_used >= encourage_from {
            Self::Encourage
        } else {
            Self::Free
        }
    }
}

/// The step count at which step-budget wrap-up starts.
fn force_start(config: &AgentConfig) -> usize {
    config.max_steps.saturating_sub(config.wrap_up_steps.max(1))
}

/// Notes already appended to the conversation of one run.
#[derive(Default)]
struct Notes {
    /// Pending hunks named by the last debt note sent.
    debt: Option<PendingSignature>,
    band: BudgetBand,
    verdict_due: bool,
}

impl Notes {
    /// Appends the notes due before the next step. Called only with the repository tools and
    /// outside wrap-up; each note stays in the conversation.
    fn append(
        &mut self,
        messages: &mut Vec<Message>,
        config: &AgentConfig,
        steps_used: usize,
        ledger: Option<&CoverageLedger>,
        submit_name: &str,
    ) {
        // Placed right after the step's tool results; the first request has none.
        if steps_used > 0
            && let Some(ledger) = ledger
            && let Some(note) = ledger.debt_note(submit_name)
        {
            let signature = ledger.pending_signature();
            if self.debt.as_ref() != Some(&signature) {
                messages.push(user_message(note));
                self.debt = Some(signature);
            }
        }
        if config.budget_bands && config.max_steps >= BUDGET_BANDS_MIN_STEPS {
            let band = BudgetBand::at(steps_used, config);
            if band > self.band {
                self.band = band;
                let max_steps = config.max_steps;
                let note = match band {
                    BudgetBand::Free => None,
                    BudgetBand::Encourage => Some(format!(
                        "Step budget: {steps_used} of {max_steps} steps used. Keep going through the changed code you have not checked yet, and batch independent lookups in one turn."
                    )),
                    BudgetBand::Urgent => Some(format!(
                        "Step budget: {} steps left before only {submit_name} is offered. Finish the check you are on, then call {submit_name} with every finding and dismissal so far.",
                        force_start(config).saturating_sub(steps_used)
                    )),
                };
                messages.extend(note.map(user_message));
            }
        }
        if config.no_tools_on_last_step && !self.verdict_due && steps_used + 2 == config.max_steps {
            self.verdict_due = true;
            messages.push(user_message(
                "One step left for reading. After it you must answer with your final JSON verdict, without tools, based on what you have read.".to_owned(),
            ));
        }
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
            && let Some(budget) = config
                .token_budget
                .as_ref()
                .filter(|budget| budget.exhausted())
        {
            // Counted so the review summary can say some reviewers stopped early.
            budget.note_wrap_up();
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

/// Steps, cache tokens, LLM time and opened files of one run.
#[derive(Default)]
struct Tally {
    steps_used: usize,
    cache_read_tokens: u32,
    cache_write_tokens: u32,
    /// Summed wall time of the LLM calls.
    llm_time: Duration,
    opened_files: HashSet<String>,
}

impl Tally {
    fn add(&mut self, response: &ChatResponse, llm_time: Duration) {
        self.steps_used += 1;
        self.llm_time = self.llm_time.saturating_add(llm_time);
        self.cache_read_tokens = self
            .cache_read_tokens
            .saturating_add(response.cache_read_tokens);
        self.cache_write_tokens = self
            .cache_write_tokens
            .saturating_add(response.cache_write_tokens);
    }

    fn finish<T>(&mut self, result: T, cut_short: bool, gate_refused: bool) -> AgentOutcome<T> {
        let mean_step = u32::try_from(self.steps_used)
            .ok()
            .filter(|&steps| steps > 0)
            .map(|steps| self.llm_time / steps);
        AgentOutcome {
            result,
            steps_used: self.steps_used,
            cache_read_tokens: self.cache_read_tokens,
            cache_write_tokens: self.cache_write_tokens,
            cut_short,
            gate_refused,
            opened_files: std::mem::take(&mut self.opened_files),
            mean_step,
        }
    }
}

/// The one-shot completion gate and the submissions it refused, oldest first.
struct CompletionGate<T> {
    enabled: bool,
    /// Parsed submissions the gate refused; the first is the held one.
    refused: Vec<T>,
}

impl<T> CompletionGate<T> {
    fn new(enabled: bool) -> Self {
        Self {
            enabled,
            refused: Vec::new(),
        }
    }

    /// The qualifying files still unread when a submission parsed in this step must be refused:
    /// the gate has not refused yet, the run is not wrapping up, at least
    /// [`GATE_MIN_STEPS_LEFT`] investigating steps remain after this one, and a qualifying hunk
    /// is unread. Reads of the current turn are not recorded yet, so they do not count.
    fn pending(
        &self,
        config: &AgentConfig,
        wrap_up_active: bool,
        steps_used: usize,
        ledger: Option<&CoverageLedger>,
    ) -> Option<Vec<String>> {
        if !self.enabled || !self.refused.is_empty() || wrap_up_active {
            return None;
        }
        if force_start(config).saturating_sub(steps_used) < GATE_MIN_STEPS_LEFT {
            return None;
        }
        let files = ledger?.pending_qualifying_files();
        (!files.is_empty()).then_some(files)
    }

    fn hold(&mut self, submission: T) {
        self.refused.push(submission);
    }

    /// The outcome for an accepted `result`. When it has no findings, it takes those of the
    /// latest refused submission that had some (a model refused once often resubmits `[]`
    /// meaning "already sent").
    fn accept<S: SubmitSpec<Output = T>>(
        mut self,
        submit: &S,
        result: T,
        tally: &mut Tally,
    ) -> AgentOutcome<T> {
        let gate_refused = !self.refused.is_empty();
        let result = if submit.has_findings(&result) {
            result
        } else {
            match self.refused.iter().rposition(|r| submit.has_findings(r)) {
                Some(index) => submit.with_findings_of(result, self.refused.swap_remove(index)),
                None => result,
            }
        };
        tally.finish(result, false, gate_refused)
    }

    /// The run ended without a parsable result: the held submission (`cut_short`) if the gate
    /// refused one, otherwise [`AgentError::Unparseable`].
    fn give_up(self, submit_name: &str, tally: &mut Tally) -> Result<AgentOutcome<T>, AgentError> {
        match self.refused.into_iter().next() {
            Some(held) => Ok(tally.finish(held, true, true)),
            None => Err(unparseable(submit_name, tally.steps_used)),
        }
    }
}

/// How one call of a turn that did not end the run is answered.
enum CallPlan {
    /// A read-only call, executed when the repository tools are available.
    Run,
    /// A terminal-tool call whose arguments were rejected; the text is shown to the model.
    Invalid(String),
    /// A terminal-tool call the completion gate refused (not an error).
    Refused(String),
}

/// Safe error text: names the terminal tool and the step count, nothing else.
fn unparseable(submit_name: &str, steps_used: usize) -> AgentError {
    AgentError::Unparseable(format!(
        "no valid {submit_name} result after {steps_used} steps"
    ))
}

/// One LLM call. A context overflow is retried once per run on a compacted conversation, as
/// the same step.
async fn request(
    llm: &AgentLlm<'_>,
    messages: &mut Vec<Message>,
    tools: Vec<ToolDef>,
    overflow_retried: &mut bool,
) -> Result<ChatResponse, AgentError> {
    let build = |messages: &[Message], tools: Vec<ToolDef>| ChatRequest {
        model: llm.model.to_owned(),
        system: llm.system.to_owned(),
        messages: messages.to_vec(),
        tools,
        max_tokens: MAX_OUTPUT_TOKENS,
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
    plans: Vec<CallPlan>,
    read_only: Option<&Toolbox>,
    submit_name: &str,
    max_concurrent: usize,
) -> Vec<ToolResult> {
    // Futures are built up front: a closure inside the stream would make the run future
    // lose `Send` (higher-ranked lifetime inference).
    let pending: Vec<_> = calls
        .iter()
        .zip(plans)
        .map(|(call, plan)| answer_call(call, plan, read_only, submit_name))
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
    plan: CallPlan,
    read_only: Option<&Toolbox>,
    submit_name: &str,
) -> ToolResult {
    let output = match (plan, read_only) {
        (CallPlan::Refused(content), _) => ToolOutput {
            content,
            is_error: false,
        },
        (CallPlan::Invalid(err), _) => ToolOutput {
            content: format!(
                "Error: invalid {submit_name} arguments: {err}. Call {submit_name} again with corrected arguments."
            ),
            is_error: true,
        },
        (CallPlan::Run, Some(toolbox)) => {
            toolbox.execute_read_only(&call.name, &call.arguments).await
        }
        (CallPlan::Run, None) => ToolOutput {
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

fn trace_enabled() -> bool {
    tracing::enabled!(target: TRACE_TARGET, tracing::Level::TRACE)
}

/// Traces the system prompt and the run's first user message.
fn trace_prompt(system: &str, messages: &[Message]) {
    if !trace_enabled() {
        return;
    }
    let prompt = messages
        .iter()
        .find_map(|m| match m {
            Message::User { parts } => parts.iter().map(|UserPart::Text(t)| t.as_str()).next(),
            _ => None,
        })
        .unwrap_or_default();
    tracing::trace!(
        target: TRACE_TARGET,
        system = %redact_secrets(system),
        prompt = %redact_secrets(prompt),
        "agent prompt"
    );
}

/// Traces what the model produced in one step: reasoning, reply text and every tool call.
fn trace_response(step: usize, response: &ChatResponse) {
    if !trace_enabled() {
        return;
    }
    if let Some(reasoning) = response.reasoning() {
        tracing::trace!(target: TRACE_TARGET, step, reasoning = %redact_secrets(&reasoning), "agent reasoning");
    }
    if !response.text.trim().is_empty() {
        tracing::trace!(target: TRACE_TARGET, step, text = %redact_secrets(&response.text), "agent text");
    }
    for call in &response.tool_calls {
        let args = call.arguments.to_string();
        tracing::trace!(
            target: TRACE_TARGET,
            step,
            tool = %call.name,
            id = %call.id,
            args = %redact_secrets(&args),
            "agent tool call"
        );
    }
}

/// Traces the result of every tool call of one step, each cut to [`TRACE_RESULT_CHARS`].
fn trace_results(step: usize, results: &[ToolResult]) {
    if !trace_enabled() {
        return;
    }
    for result in results {
        let content = truncate_with_marker(&result.content, TRACE_RESULT_CHARS, TRUNCATED_MARKER);
        tracing::trace!(
            target: TRACE_TARGET,
            step,
            tool = %result.tool_name,
            id = %result.call_id,
            is_error = result.is_error,
            content = %redact_secrets(&content),
            "agent tool result"
        );
    }
}

fn user_message(text: String) -> Message {
    Message::User {
        parts: vec![UserPart::Text(text)],
    }
}

fn assistant_message(text: &str, calls: &[ToolCall], provider_meta: Option<String>) -> Message {
    let mut parts = Vec::with_capacity(calls.len() + usize::from(provider_meta.is_some()) + 1);
    if let Some(meta) = provider_meta {
        parts.push(AssistantPart::ProviderMeta(meta));
    }
    if !text.is_empty() {
        parts.push(AssistantPart::Text(text.to_owned()));
    }
    parts.extend(calls.iter().cloned().map(AssistantPart::ToolCall));
    Message::Assistant { parts }
}

/// Shortens the oldest tool outputs longer than `COMPACT_EXCERPT_CHARS` plus
/// `COMPACTED_MARKER` (saving at least one character), oldest first, until the conversation fits
/// `limit`. The prompt and the last `COMPACT_KEEP_TAIL` messages are never touched, and only
/// `ToolResult.content` changes, so call/result pairing holds. One pass: when nothing is left to
/// shorten it stops, even above the limit.
fn compact(messages: &mut [Message], limit: usize) {
    let mut total = messages_char_len(messages);
    if total <= limit {
        return;
    }
    let protected_from = messages.len().saturating_sub(COMPACT_KEEP_TAIL);
    let Some(candidates) = messages.get_mut(1..protected_from) else {
        return;
    };
    let min_compact_chars = COMPACT_EXCERPT_CHARS + COMPACTED_MARKER.chars().count();
    for message in candidates {
        let Message::Tool { results } = message else {
            continue;
        };
        for result in results {
            if total <= limit {
                return;
            }
            let len = result.content.chars().count();
            if len <= min_compact_chars || result.content.ends_with(COMPACTED_MARKER) {
                continue;
            }
            if let Cow::Owned(short) =
                truncate_with_marker(&result.content, COMPACT_EXCERPT_CHARS, COMPACTED_MARKER)
            {
                let new_len = short.chars().count();
                result.content = short;
                total = total.saturating_sub(len.saturating_sub(new_len));
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
                if let Cow::Owned(cut) =
                    truncate_with_marker(&result.content, OVERFLOW_TOOL_CHARS, TRUNCATED_MARKER)
                {
                    result.content = cut;
                }
            }
        }
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
                // Gemini thought signatures, OpenAI-compatible reasoning), so it counts toward
                // the context size.
                AssistantPart::ToolCall(call) => {
                    call.name.chars().count()
                        + call.arguments.to_string().chars().count()
                        + call
                            .provider_meta
                            .as_ref()
                            .map_or(0, |meta| meta.chars().count())
                }
                AssistantPart::ProviderMeta(meta) => meta.chars().count(),
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
    use std::sync::Arc;
    use std::time::Duration;

    use adus_git::{FileDiff, FileStatus};
    use adus_llm::{
        AssistantPart, ChatRequest, ChatResponse, ReasoningEffort, ToolCall, ToolResult, UserPart,
    };
    use serde_json::json;

    use super::*;
    use crate::test_support::{ScriptedChat, call, msg_text, text_turn, tool_names, tools_turn};
    use crate::workspace::RepoWorkspace;

    const SUBMIT: &str = "submit_answer";
    const REPORT: &str = "submit_findings";
    const GATE_REFUSAL: &str = "Not submitted yet. Before you submit, open these changed files with read_file and check the code around each change: src/a.rs. Then call submit_findings again with every finding you confirmed. This is asked only once; your next submit_findings call will be accepted.";
    const VERDICT_DUE: &str = "One step left for reading. After it you must answer with your final JSON verdict, without tools, based on what you have read.";
    const NUDGE: &str =
        "Please submit your result by calling submit_answer (or return the JSON directly).";
    const WRAP_UP: &str =
        "You are near the end of your budget. Submit your final result now using submit_answer.";
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

        fn parse_arguments(&self, arguments: &Value, _can_retry: bool) -> Result<String, String> {
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
            self.parse_arguments(&value, false)
        }
    }

    fn submit(id: &str, answer: &str) -> ToolCall {
        call(id, SUBMIT, json!({"answer": answer}))
    }

    fn read(id: &str, path: &str) -> ToolCall {
        call(id, "read_file", json!({"path": path}))
    }

    fn toolbox(files: &[(&str, &str)]) -> Toolbox {
        let raw: Vec<(&str, &[u8])> = files.iter().map(|(p, b)| (*p, b.as_bytes())).collect();
        Toolbox::new(Arc::new(RepoWorkspace::from_files(&raw)))
    }

    fn default_toolbox() -> Toolbox {
        toolbox(&[
            ("src/a.rs", "fn alpha() {}\n"),
            ("src/b.rs", "fn beta() {}\n"),
        ])
    }

    /// Toy findings tool shaped like `submit_findings`: `{"reasoning": "..", "findings": [..]}`.
    struct SubmitReport {
        def: ToolDef,
    }

    #[derive(Debug, Clone, PartialEq)]
    struct Report {
        reasoning: String,
        findings: Vec<String>,
    }

    impl SubmitReport {
        fn new() -> Self {
            Self {
                def: ToolDef {
                    name: REPORT.into(),
                    description: "Submit the findings.".into(),
                    parameters: json!({"type": "object"}),
                },
            }
        }
    }

    impl SubmitSpec for SubmitReport {
        type Output = Report;

        fn tool_def(&self) -> &ToolDef {
            &self.def
        }

        fn parse_arguments(&self, arguments: &Value, _can_retry: bool) -> Result<Report, String> {
            let findings = arguments
                .get("findings")
                .and_then(Value::as_array)
                .ok_or("missing array field `findings`")?;
            Ok(Report {
                reasoning: arguments
                    .get("reasoning")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                findings: findings
                    .iter()
                    .filter_map(|f| f.as_str().map(str::to_owned))
                    .collect(),
            })
        }

        fn parse_prose(&self, text: &str) -> Result<Report, String> {
            let start = text.find('{').ok_or("no JSON object")?;
            let end = text.rfind('}').ok_or("no JSON object")?;
            let value: Value = serde_json::from_str(text.get(start..=end).ok_or("no JSON object")?)
                .map_err(|e| e.to_string())?;
            self.parse_arguments(&value, false)
        }

        fn has_findings(&self, output: &Report) -> bool {
            !output.findings.is_empty()
        }

        fn with_findings_of(&self, accepted: Report, earlier: Report) -> Report {
            Report {
                findings: earlier.findings,
                ..accepted
            }
        }
    }

    fn report(id: &str, reasoning: &str, findings: &[&str]) -> ToolCall {
        call(
            id,
            REPORT,
            json!({"reasoning": reasoning, "findings": findings}),
        )
    }

    fn read_lines(id: &str, path: &str, start: u32, end: u32) -> ToolCall {
        call(
            id,
            "read_file",
            json!({"path": path, "start_line": start, "end_line": end}),
        )
    }

    /// A coverage ledger over `(path, hunks)` files.
    fn ledger(files: &[(&str, &[(u32, u32)])]) -> CoverageLedger {
        let diffs: Vec<FileDiff> = files
            .iter()
            .map(|(path, hunks)| FileDiff {
                path: (*path).to_owned(),
                old_path: None,
                status: FileStatus::Modified,
                patch: Some(
                    hunks
                        .iter()
                        .map(|&(start, end)| {
                            let count = end - start + 1;
                            format!(
                                "@@ -{start},0 +{start},{count} @@
{}",
                                "+x
"
                                .repeat(count as usize)
                            )
                        })
                        .collect(),
                ),
            })
            .collect();
        CoverageLedger::from_files(&diffs.iter().collect::<Vec<_>>())
    }

    /// A toolbox holding `src/a.rs` with 100 numbered lines.
    fn hundred_line_toolbox() -> Toolbox {
        let body: String = (1..=100)
            .map(|i| {
                format!(
                    "line {i}
"
                )
            })
            .collect();
        toolbox(&[("src/a.rs", &body)])
    }

    /// Gated config: a ledger over `src/a.rs` lines 50-52 and the completion gate on.
    fn gated(max_steps: usize) -> AgentConfig {
        AgentConfig {
            coverage_ledger: Some(ledger(&[("src/a.rs", &[(50, 52)])])),
            enforce_gate: true,
            ..AgentConfig::new(max_steps)
        }
    }

    fn debt_notes(req: &ChatRequest) -> Vec<&str> {
        req.messages
            .iter()
            .filter_map(msg_text)
            .filter(|t| t.starts_with("Unread changed code."))
            .collect()
    }

    /// Every request repeats the previous request's messages unchanged, then appends.
    fn assert_append_only(reqs: &[ChatRequest]) {
        for pair in reqs.windows(2) {
            let (before, after) = (&pair[0].messages, &pair[1].messages);
            assert!(after.len() > before.len());
            assert_eq!(&after[..before.len()], &before[..], "history was rewritten");
        }
    }

    async fn run_with<S: SubmitSpec>(
        chat: &ScriptedChat,
        tools: AgentTools<'_>,
        submit: &S,
        config: &AgentConfig,
    ) -> Result<AgentOutcome<S::Output>, AgentError> {
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
        run_react_loop(&llm, tools, submit, config, "PROMPT".into()).await
    }

    async fn run(
        chat: &ScriptedChat,
        tools: AgentTools<'_>,
        config: &AgentConfig,
    ) -> Result<AgentOutcome<String>, AgentError> {
        run_with(chat, tools, &SubmitAnswer::new(), config).await
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
                    AssistantPart::Text(_) | AssistantPart::ProviderMeta(_) => None,
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

    fn count_user(req: &ChatRequest, text: &str) -> usize {
        req.messages
            .iter()
            .filter(|m| msg_text(m) == Some(text))
            .count()
    }

    fn count_notes(req: &ChatRequest) -> usize {
        req.messages
            .iter()
            .filter(|m| msg_text(m).is_some_and(|t| t.starts_with("Step budget: ")))
            .count()
    }

    fn with_cache(mut turn: Result<ChatResponse, ChatError>) -> Result<ChatResponse, ChatError> {
        if let Ok(resp) = &mut turn {
            resp.cache_read_tokens = 7;
            resp.cache_write_tokens = 3;
        }
        turn
    }

    #[tokio::test]
    async fn completes_multi_turn_tool_calls_then_submits() {
        let tb = default_toolbox();
        let chat = ScriptedChat::new(vec![
            with_cache(tools_turn(vec![read("c1", "src/a.rs")])),
            with_cache(tools_turn(vec![submit("c2", "done")])),
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
        assert_eq!(reqs[0].max_tokens, MAX_OUTPUT_TOKENS);
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

        let calls = chat.calls();
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
    async fn retries_malformed_submit_and_keeps_multi_call_order() {
        let tb = default_toolbox();
        let chat = ScriptedChat::new(vec![
            tools_turn(vec![call("c1", SUBMIT, json!({"wrong": 1}))]),
            tools_turn(vec![
                read("r1", "src/a.rs"),
                call("s1", SUBMIT, json!("not an object")),
                call("g1", "grep", json!({"pattern": "beta"})),
            ]),
            tools_turn(vec![submit("c2", "fixed")]),
        ]);
        let out = run(&chat, AgentTools::Full(&tb), &AgentConfig::new(6))
            .await
            .unwrap();
        assert_eq!(out.result, "fixed");
        assert_eq!(out.steps_used, 3);

        let reqs = chat.requests();
        let single = tool_results(&reqs[1].messages[2]);
        assert_eq!(single.len(), 1);
        assert_eq!(single[0].call_id, "c1");
        assert_eq!(single[0].tool_name, SUBMIT);
        assert!(single[0].is_error);
        assert_eq!(
            single[0].content,
            "Error: invalid submit_answer arguments: missing string field `answer`. Call submit_answer again with corrected arguments."
        );

        assert_paired(&reqs[2].messages);
        let multi = tool_results(&reqs[2].messages[4]);
        let ids: Vec<&str> = multi.iter().map(|r| r.call_id.as_str()).collect();
        assert_eq!(ids, ["r1", "s1", "g1"]);
        assert!(!multi[0].is_error);
        assert!(multi[1].is_error);
        assert_eq!(multi[1].tool_name, SUBMIT);
        assert!(!multi[2].is_error);
        assert!(multi[2].content.contains("beta"));
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
    async fn wraps_up_with_submit_only_in_final_window() {
        // Default 1-step wrap-up across budgets, including a successful wrap-up submit.
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
        assert_eq!(tool_names(&reqs[2]), [SUBMIT]);
        assert_eq!(msg_text(reqs[2].messages.last().unwrap()), Some(WRAP_UP));

        for max_steps in [1usize, 2, 6] {
            let script = (0..max_steps)
                .map(|i| tools_turn(vec![read(&format!("c{i}"), "src/a.rs")]))
                .collect();
            let chat = ScriptedChat::new(script);
            let err = run(&chat, AgentTools::Full(&tb), &AgentConfig::new(max_steps))
                .await
                .unwrap_err();
            assert!(matches!(err, AgentError::Unparseable(_)), "{max_steps}");
            let reqs = chat.requests();
            assert_eq!(reqs.len(), max_steps);
            for (step, req) in reqs.iter().enumerate() {
                let final_step = step + 1 == max_steps;
                assert_eq!(count_user(req, WRAP_UP), usize::from(final_step));
                assert_eq!(tool_names(req).len(), if final_step { 1 } else { 5 });
                assert_paired(&req.messages);
            }
        }

        // Multi-step wrap-up window refuses read-only calls inside the window.
        let mut script: Vec<_> = (1..=5)
            .map(|i| tools_turn(vec![read(&format!("c{i}"), "src/a.rs")]))
            .collect();
        script.push(tools_turn(vec![submit("c6", "wrapped")]));
        let chat = ScriptedChat::new(script);
        let mut config = AgentConfig::new(6);
        config.wrap_up_steps = 2;
        run(&chat, AgentTools::Full(&tb), &config).await.unwrap();
        let reqs = chat.requests();
        for (step, req) in reqs.iter().enumerate() {
            let wrap_up = step >= 4;
            assert_eq!(tool_names(req).len(), if wrap_up { 1 } else { 5 });
            assert_eq!(count_user(req, WRAP_UP), usize::from(wrap_up));
            assert_paired(&req.messages);
        }
        let refused = tool_results(reqs[5].messages.last().unwrap());
        assert!(refused[0].is_error);
        assert_eq!(
            refused[0].content,
            "Error: tool read_file is not available now; call submit_answer with your result"
        );

        // Window larger than budget makes every step a wrap-up step.
        let chat = ScriptedChat::new(vec![
            tools_turn(vec![read("c1", "src/a.rs")]),
            tools_turn(vec![submit("c2", "ok")]),
        ]);
        let mut config = AgentConfig::new(3);
        config.wrap_up_steps = 5;
        run(&chat, AgentTools::Full(&tb), &config).await.unwrap();
        let reqs = chat.requests();
        assert!(reqs.iter().all(|r| tool_names(r) == [SUBMIT]));
        assert!(reqs.iter().all(|r| count_user(r, WRAP_UP) == 1));
    }

    #[tokio::test]
    async fn exhausted_step_budget_recovers_prose_or_fails_safely() {
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

        let chat = ScriptedChat::new(vec![
            tools_turn(vec![call("c0", SUBMIT, json!({}))]),
            tools_turn(vec![call("c1", SUBMIT, json!({}))]),
        ]);
        let AgentError::Unparseable(message) =
            run(&chat, AgentTools::Full(&tb), &AgentConfig::new(2))
                .await
                .unwrap_err()
        else {
            panic!("expected Unparseable");
        };
        assert!(!message.contains("PROMPT"));
    }

    #[tokio::test]
    async fn prose_fallback_and_single_nudge_behavior() {
        let tb = default_toolbox();
        // Immediate prose JSON fallback succeeds on step 1.
        let chat = ScriptedChat::new(vec![text_turn(
            "Here is my answer:\n```json\n{\"answer\": \"prose\"}\n```",
        )]);
        let out = run(&chat, AgentTools::Full(&tb), &AgentConfig::new(20))
            .await
            .unwrap();
        assert_eq!(out.result, "prose");
        assert_eq!(out.steps_used, 1);
        assert!(!out.cut_short);

        // Nudge once, then accept submit.
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
        assert_eq!(msg_text(reqs[1].messages.last().unwrap()), Some(NUDGE));
        assert_eq!(reqs[1].messages.len(), 3);

        // Nudges at most once per run, and never when max_steps == 1.
        let chat = ScriptedChat::new(vec![
            text_turn("no json here"),
            tools_turn(vec![read("c1", "src/a.rs")]),
            text_turn("still no json"),
        ]);
        let err = run(&chat, AgentTools::Full(&tb), &AgentConfig::new(20))
            .await
            .unwrap_err();
        assert!(matches!(err, AgentError::Unparseable(_)));
        assert_eq!(count_user(&chat.requests()[2], NUDGE), 1);

        let chat = ScriptedChat::new(vec![text_turn("no json"), text_turn("unused")]);
        let err = run(&chat, AgentTools::Full(&tb), &AgentConfig::new(1))
            .await
            .unwrap_err();
        assert!(matches!(err, AgentError::Unparseable(_)));
        assert_eq!(chat.requests().len(), 1);
    }

    #[tokio::test]
    async fn nudge_and_no_tools_modes_skip_wrap_up_reminder() {
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
        assert_eq!(msg_text(reqs[1].messages.last().unwrap()), Some(NUDGE));
        assert_eq!(count_user(&reqs[1], WRAP_UP), 0);

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
        assert!(reqs.iter().all(|r| r.tools.is_empty()));
        assert_eq!(msg_text(reqs[1].messages.last().unwrap()), Some(NUDGE));
        assert_eq!(count_user(&reqs[1], WRAP_UP), 0);
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
    async fn compacts_oldest_tool_outputs_in_run_when_exceeding_char_limit() {
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
        // The oldest output is shortened to a 400-char excerpt; the last 4 messages stay whole.
        let excerpt: String = tool_results(&msgs[4])[0]
            .content
            .chars()
            .take(COMPACT_EXCERPT_CHARS)
            .collect();
        let expected_short = format!("{excerpt}{COMPACTED_MARKER}");
        assert_eq!(tool_results(&msgs[2])[0].content, expected_short);
        assert!(tool_results(&msgs[4])[0].content.contains(&big));
        assert!(tool_results(&msgs[6])[0].content.contains(&big));
    }

    #[test]
    fn compact_respects_limit_protects_tail_and_terminates() {
        assert_eq!(AgentConfig::new(20).compaction_char_limit, 320_000);
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
        compact(&mut msgs, messages_char_len(&before) - 100);
        let expected_short = format!("{}{COMPACTED_MARKER}", "y".repeat(COMPACT_EXCERPT_CHARS));
        assert_eq!(tool_results(&msgs[2])[0].content, expected_short);
        assert_eq!(msgs[3..], before[3..]);
        assert_eq!(msgs[0], before[0]);

        let at_guard = "z".repeat(446);
        let already_marked = format!("{}{COMPACTED_MARKER}", "z".repeat(500));
        let mut protected = vec![
            user(&long),
            assistant_calls(&["a", "b", "c"]),
            Message::Tool {
                results: vec![
                    ToolResult {
                        call_id: "a".into(),
                        tool_name: "read_file".into(),
                        content: "short output".into(),
                        is_error: false,
                    },
                    ToolResult {
                        call_id: "b".into(),
                        tool_name: "read_file".into(),
                        content: at_guard,
                        is_error: false,
                    },
                    ToolResult {
                        call_id: "c".into(),
                        tool_name: "read_file".into(),
                        content: already_marked,
                        is_error: false,
                    },
                ],
            },
            assistant_calls(&["d"]),
            tool_msg(&["d"], &long),
            assistant_calls(&["e"]),
            tool_msg(&["e"], &long),
        ];
        let before = protected.clone();
        compact(&mut protected, 0);
        assert_eq!(protected, before);
    }

    #[test]
    fn compact_for_overflow_keeps_prompt_latest_turn_and_trailing_user() {
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
    async fn recovers_once_from_context_overflow_and_fails_on_second() {
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
        let retried = &reqs[3].messages;
        assert_eq!(retried.len(), 3);
        assert_eq!(retried[0], user("PROMPT"));
        assert_eq!(retried[1], reqs[2].messages[3]);
        assert_paired(retried);
        assert!(tool_results(&retried[2])[0].content.chars().count() < 600);
        assert_eq!(tool_names(&reqs[3]), tool_names(&reqs[2]));

        let tb = default_toolbox();
        let chat = ScriptedChat::new(vec![
            tools_turn(vec![read("c1", "src/a.rs")]),
            Err(ChatError::ContextOverflow("too long".into())),
            Err(ChatError::ContextOverflow("still too long".into())),
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
    async fn propagates_tools_unsupported_and_chat_errors() {
        let tb = default_toolbox();
        let chat = ScriptedChat::new(vec![Err(ChatError::ToolsUnsupported(
            "no tools here".into(),
        ))]);
        let err = run(&chat, AgentTools::Full(&tb), &AgentConfig::new(20))
            .await
            .unwrap_err();
        assert_eq!(err, AgentError::ToolsUnsupported("no tools here".into()));
        assert_eq!(chat.requests().len(), 1);

        let chat = ScriptedChat::new(vec![Err(ChatError::NoKeys)]);
        let err = run(&chat, AgentTools::SubmitOnly, &AgentConfig::new(6))
            .await
            .unwrap_err();
        assert_eq!(err, AgentError::Chat(ChatError::NoKeys));
        assert_eq!(chat.requests().len(), 1);
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
        assert!(Instant::now() <= deadline);

        let reqs = chat.requests();
        for req in &reqs[..3] {
            assert_eq!(tool_names(req).len(), 5);
            assert_eq!(count_user(req, WRAP_UP), 0);
        }
        assert_eq!(tool_names(&reqs[3]), [SUBMIT]);
        assert_eq!(msg_text(reqs[3].messages.last().unwrap()), Some(WRAP_UP));
        assert_eq!(count_user(&reqs[3], WRAP_UP), 1);
        assert_paired(&reqs[3].messages);
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
        assert_eq!(tool_names(&reqs[1]), [SUBMIT]);
        assert_eq!(count_user(&reqs[1], WRAP_UP), 1);
        assert_paired(&reqs[1].messages);
    }

    #[tokio::test]
    async fn budget_bands_emit_encourage_and_urgent_once() {
        let tb = default_toolbox();
        let reads = |n: usize| -> Vec<_> {
            (1..=n)
                .map(|i| tools_turn(vec![read(&format!("c{i}"), "src/a.rs")]))
                .collect()
        };
        let encourage = "Step budget: 3 of 12 steps used. Keep going through the changed code you have not checked yet, and batch independent lookups in one turn.";
        let urgent = "Step budget: 3 steps left before only submit_answer is offered. Finish the check you are on, then call submit_answer with every finding and dismissal so far.";

        let mut script = reads(11);
        script.push(tools_turn(vec![submit("c12", "ok")]));
        let chat = ScriptedChat::new(script);
        let config = AgentConfig {
            wrap_up_steps: 2,
            budget_bands: true,
            ..AgentConfig::new(12)
        };
        run(&chat, AgentTools::Full(&tb), &config).await.unwrap();
        let reqs = chat.requests();
        assert_eq!(reqs.len(), 12);
        for (step, req) in reqs.iter().enumerate() {
            assert_eq!(count_user(req, encourage), usize::from(step >= 3), "{step}");
            assert_eq!(count_user(req, urgent), usize::from(step >= 7), "{step}");
            assert_eq!(
                count_notes(req),
                usize::from(step >= 3) + usize::from(step >= 7)
            );
        }
        assert_eq!(msg_text(reqs[3].messages.last().unwrap()), Some(encourage));
        assert_eq!(msg_text(reqs[7].messages.last().unwrap()), Some(urgent));
        for note in [encourage, urgent] {
            assert!(!note.contains("as soon as") && !note.contains("extra steps"));
        }
        assert_append_only(&reqs);

        // No band note under 6 steps, with `budget_bands` off (a 7-step Verifier-shaped run),
        // or without repository tools.
        for (tools, config) in [
            (
                AgentTools::Full(&tb),
                AgentConfig {
                    budget_bands: true,
                    ..AgentConfig::new(5)
                },
            ),
            (AgentTools::Full(&tb), AgentConfig::new(7)),
            (
                AgentTools::SubmitOnly,
                AgentConfig {
                    wrap_up_steps: 2,
                    budget_bands: true,
                    ..AgentConfig::new(12)
                },
            ),
        ] {
            let steps = config.max_steps;
            let chat = ScriptedChat::new(reads(steps));
            let err = run(&chat, tools, &config).await.unwrap_err();
            assert!(matches!(err, AgentError::Unparseable(_)), "{err:?}");
            let reqs = chat.requests();
            assert_eq!(reqs.len(), steps);
            assert!(reqs.iter().all(|r| count_notes(r) == 0), "{steps} steps");
        }
    }

    #[tokio::test]
    async fn coverage_debt_note_is_append_only_and_sent_on_change() {
        let tb = hundred_line_toolbox();
        let grep = |id: &str| call(id, "grep", json!({"pattern": "line"}));
        let chat = ScriptedChat::new(vec![
            tools_turn(vec![grep("g1")]),
            tools_turn(vec![grep("g2")]),
            tools_turn(vec![read_lines("r1", "src/a.rs", 1, 10)]),
            tools_turn(vec![read_lines("r2", "./src/a.rs", 40, 60)]),
            tools_turn(vec![report("s1", "done", &[])]),
        ]);
        let config = AgentConfig {
            coverage_ledger: Some(ledger(&[("src/a.rs", &[(1, 3), (50, 52)])])),
            ..AgentConfig::new(20)
        };
        run_with(&chat, AgentTools::Full(&tb), &SubmitReport::new(), &config)
            .await
            .unwrap();
        let reqs = chat.requests();
        let both = "Unread changed code. You have not opened these changed lines with read_file yet:\n- src/a.rs: lines 1-3, 50-52\nRead them before you call submit_findings. A grep or list_dir match does not count as reading them.";
        let second = both.replace("lines 1-3, 50-52", "lines 50-52");
        // The first request is the prompt alone; the note follows the first step's results.
        assert!(debt_notes(&reqs[0]).is_empty());
        assert_eq!(debt_notes(&reqs[1]), [both]);
        assert_eq!(msg_text(reqs[1].messages.last().unwrap()), Some(both));
        // A grep covers nothing: no new note.
        assert_eq!(debt_notes(&reqs[2]), [both]);
        // Reading lines 1-10 covers the first hunk only.
        assert_eq!(debt_notes(&reqs[3]), [both, second.as_str()]);
        // Nothing pending: no further note, earlier notes stay.
        assert_eq!(debt_notes(&reqs[4]), [both, second.as_str()]);
        assert_append_only(&reqs);

        // Never during wrap-up (every step after the first wraps up here).
        let chat = ScriptedChat::new(vec![
            tools_turn(vec![grep("g1")]),
            tools_turn(vec![grep("g2")]),
            tools_turn(vec![report("s1", "done", &[])]),
        ]);
        let config = AgentConfig {
            max_steps: 3,
            wrap_up_steps: 2,
            ..config.clone()
        };
        run_with(&chat, AgentTools::Full(&tb), &SubmitReport::new(), &config)
            .await
            .unwrap();
        assert!(chat.requests().iter().all(|r| debt_notes(r).is_empty()));

        // Docs and tests carry no debt.
        let chat = ScriptedChat::new(vec![
            tools_turn(vec![grep("g1")]),
            tools_turn(vec![report("s1", "done", &[])]),
        ]);
        let config = AgentConfig {
            coverage_ledger: Some(ledger(&[
                ("README.md", &[(1, 3)]),
                ("tests/e2e.rs", &[(1, 3)]),
            ])),
            ..AgentConfig::new(20)
        };
        run_with(&chat, AgentTools::Full(&tb), &SubmitReport::new(), &config)
            .await
            .unwrap();
        assert!(chat.requests().iter().all(|r| debt_notes(r).is_empty()));
    }

    #[tokio::test]
    async fn gate_refuses_early_submit_and_recovers_held_on_unparseable() {
        let tb = hundred_line_toolbox();
        let spec = SubmitReport::new();

        // (a) A read in the same turn as the submit does not count: refused once, not an
        // error; the next submit is accepted.
        let chat = ScriptedChat::new(vec![
            tools_turn(vec![
                read_lines("r1", "src/a.rs", 40, 60),
                report("s1", "early", &["f1"]),
            ]),
            tools_turn(vec![report("s2", "later", &["f2"])]),
        ]);
        let out = run_with(&chat, AgentTools::Full(&tb), &spec, &gated(20))
            .await
            .unwrap();
        assert_eq!(out.result.findings, ["f2"]);
        assert!(out.gate_refused && !out.cut_short);
        assert_eq!(out.opened_files, HashSet::from(["src/a.rs".to_owned()]));
        let results = tool_results(chat.requests()[1].messages.last().unwrap()).to_vec();
        assert!(!results[0].is_error);
        assert_eq!(results[1].tool_name, REPORT);
        assert!(!results[1].is_error);
        assert_eq!(results[1].content, GATE_REFUSAL);

        // (b) A failed read does not satisfy the gate; (c) two submits of one turn are both
        // refused.
        let chat = ScriptedChat::new(vec![
            tools_turn(vec![read_lines("r1", "src/a.rs", 60, 50)]),
            tools_turn(vec![
                report("s1", "one", &["f1"]),
                report("s2", "two", &["f2"]),
            ]),
            tools_turn(vec![report("s3", "three", &["f3"])]),
        ]);
        let out = run_with(&chat, AgentTools::Full(&tb), &spec, &gated(20))
            .await
            .unwrap();
        assert_eq!(out.result.findings, ["f3"]);
        assert!(out.opened_files.is_empty());
        let reqs = chat.requests();
        assert!(tool_results(&reqs[1].messages[2])[0].is_error);
        let refused = tool_results(reqs[2].messages.last().unwrap());
        assert_eq!(refused.len(), 2);
        assert!(
            refused
                .iter()
                .all(|r| !r.is_error && r.content == GATE_REFUSAL)
        );

        // (d) Early prose JSON is refused with a user message; (e) when the run then ends
        // unparseable, the held submission is returned.
        let chat = ScriptedChat::new(vec![
            text_turn(r#"{"reasoning": "held", "findings": ["h1"]}"#),
            text_turn("no json"),
            text_turn("still no json"),
        ]);
        let out = run_with(&chat, AgentTools::Full(&tb), &spec, &gated(20))
            .await
            .unwrap();
        assert_eq!(
            out.result,
            Report {
                reasoning: "held".into(),
                findings: vec!["h1".into()],
            }
        );
        assert!(out.cut_short && out.gate_refused);
        let reqs = chat.requests();
        assert_eq!(reqs.len(), 3);
        assert_eq!(count_user(&reqs[1], GATE_REFUSAL), 1);
        assert_eq!(count_user(&reqs[2], &NUDGE.replace(SUBMIT, REPORT)), 1);
    }

    #[tokio::test]
    async fn gate_skipped_when_insufficient_steps_or_no_tools() {
        let tb = hundred_line_toolbox();
        let spec = SubmitReport::new();
        let early = || {
            ScriptedChat::new(vec![
                tools_turn(vec![report("s1", "r", &["f1"])]),
                tools_turn(vec![report("s2", "r", &["f2"])]),
            ])
        };

        // 4 steps: 4 - 1 - 1 = 2 investigating steps left, so the gate refuses.
        let chat = early();
        let out = run_with(&chat, AgentTools::Full(&tb), &spec, &gated(4))
            .await
            .unwrap();
        assert_eq!(out.result.findings, ["f2"]);
        assert!(out.gate_refused);

        // 3 steps (3 - 1 - 1 < 2), a 2-step wrap-up, or no repository tools: accepted at once.
        let mut window = gated(4);
        window.wrap_up_steps = 2;
        for (tools, config) in [
            (AgentTools::Full(&tb), gated(3)),
            (AgentTools::Full(&tb), window),
            (AgentTools::SubmitOnly, gated(20)),
        ] {
            let chat = early();
            let out = run_with(&chat, tools, &spec, &config).await.unwrap();
            assert_eq!(out.result.findings, ["f1"]);
            assert!(!out.gate_refused);
            assert_eq!(out.steps_used, 1);
        }
    }

    #[tokio::test]
    async fn gate_empty_resubmit_keeps_earlier_findings() {
        let tb = hundred_line_toolbox();
        let chat = ScriptedChat::new(vec![
            tools_turn(vec![report("s1", "first", &["a", "b"])]),
            tools_turn(vec![read_lines("r1", "src/a.rs", 40, 60)]),
            tools_turn(vec![report("s2", "second", &[])]),
        ]);
        let out = run_with(
            &chat,
            AgentTools::Full(&tb),
            &SubmitReport::new(),
            &gated(20),
        )
        .await
        .unwrap();
        assert_eq!(
            out.result,
            Report {
                reasoning: "second".into(),
                findings: vec!["a".into(), "b".into()],
            }
        );
        assert!(out.gate_refused && !out.cut_short);
        assert_eq!(out.steps_used, 3);
    }

    #[tokio::test]
    async fn no_tools_on_last_step_strips_tools_and_accepts_prose() {
        let tb = default_toolbox();
        let chat = ScriptedChat::new(vec![
            tools_turn(vec![read("c1", "src/a.rs")]),
            tools_turn(vec![read("c2", "src/b.rs")]),
            text_turn(r#"Verdict: {"answer": "final"}"#),
        ]);
        let config = AgentConfig {
            no_tools_on_last_step: true,
            ..AgentConfig::new(3)
        };
        let out = run(&chat, AgentTools::Full(&tb), &config).await.unwrap();
        assert_eq!(out.result, "final");
        assert!(!out.cut_short);
        assert_eq!(out.steps_used, 3);
        assert!(out.mean_step.is_some());
        assert_eq!(
            out.opened_files,
            HashSet::from(["src/a.rs".to_owned(), "src/b.rs".to_owned()])
        );

        let reqs = chat.requests();
        assert_eq!(tool_names(&reqs[0]).len(), 5);
        assert_eq!(tool_names(&reqs[1]).len(), 5);
        assert!(reqs[2].tools.is_empty());
        assert_eq!(
            msg_text(reqs[1].messages.last().unwrap()),
            Some(VERDICT_DUE)
        );
        assert_eq!(count_user(&reqs[1], VERDICT_DUE), 1);
        assert_eq!(count_user(&reqs[2], VERDICT_DUE), 1);
        assert_eq!(
            msg_text(reqs[2].messages.last().unwrap()),
            Some(
                "This is your last step and no tools are available. Reply with only your final result as the JSON object you would pass to submit_answer, based on what you have read."
            )
        );
        assert_eq!(count_user(&reqs[2], WRAP_UP), 0);
        assert_append_only(&reqs);
    }

    #[tokio::test]
    async fn widen_short_reads_expands_narrow_line_ranges() {
        let tb = hundred_line_toolbox();
        let chat = ScriptedChat::new(vec![
            tools_turn(vec![
                read_lines("r1", "src/a.rs", 30, 35),
                read_lines("r2", "src/a.rs", 5, 8),
                read_lines("r3", "src/a.rs", 30, 60),
            ]),
            tools_turn(vec![submit("s1", "ok")]),
        ]);
        let config = AgentConfig {
            widen_short_reads: true,
            ..AgentConfig::new(20)
        };
        run(&chat, AgentTools::Full(&tb), &config).await.unwrap();
        let reqs = chat.requests();
        let results = tool_results(&reqs[1].messages[2]);
        let headers: Vec<&str> = results
            .iter()
            .map(|r| r.content.lines().next().unwrap())
            .collect();
        assert_eq!(
            headers,
            [
                "[src/a.rs: lines 10-45 of 100]",
                "[src/a.rs: lines 1-18 of 100]",
                "[src/a.rs: lines 30-60 of 100]",
            ]
        );
        // The conversation keeps the call as the model sent it.
        assert_eq!(
            reqs[1].messages[1],
            Message::Assistant {
                parts: vec![
                    AssistantPart::ToolCall(read_lines("r1", "src/a.rs", 30, 35)),
                    AssistantPart::ToolCall(read_lines("r2", "src/a.rs", 5, 8)),
                    AssistantPart::ToolCall(read_lines("r3", "src/a.rs", 30, 60)),
                ],
            }
        );
    }

    #[tokio::test]
    async fn exhausted_budget_switches_agent_to_submit_only() {
        let tb = default_toolbox();
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
        assert_eq!(budget.used(), 400);
        assert_eq!(budget.wrap_ups(), 1);

        let reqs = chat.requests();
        for req in &reqs[..3] {
            assert_eq!(tool_names(req).len(), 5);
            assert_eq!(count_user(req, WRAP_UP), 0);
        }
        assert_eq!(tool_names(&reqs[3]), [SUBMIT]);
        assert_eq!(msg_text(reqs[3].messages.last().unwrap()), Some(WRAP_UP));
        assert_paired(&reqs[3].messages);

        // Already-exhausted budget still allows one investigating step, then submits.
        let chat = ScriptedChat::new(vec![
            tools_turn(vec![read("c1", "src/a.rs")]),
            tools_turn(vec![submit("c2", "late")]),
        ]);
        let out = run(&chat, AgentTools::Full(&tb), &config).await.unwrap();
        assert_eq!(out.result, "late");
        let reqs = chat.requests();
        assert_eq!(tool_names(&reqs[0]).len(), 5);
        assert_eq!(tool_names(&reqs[1]), [SUBMIT]);
        assert_eq!(budget.wrap_ups(), 2);

        // Step-budget wrap-up and failed calls do not increment wrap_ups / used.
        let chat = ScriptedChat::new(vec![
            tools_turn(vec![read("c1", "src/a.rs")]),
            tools_turn(vec![submit("c2", "by steps")]),
        ]);
        let unused = Arc::new(TokenBudget::new(0));
        let mut config = AgentConfig::new(2);
        config.token_budget = Some(Arc::clone(&unused));
        run(&chat, AgentTools::Full(&tb), &config).await.unwrap();
        assert_eq!(unused.wrap_ups(), 0);

        let chat = ScriptedChat::new(vec![Err(ChatError::NoKeys)]);
        let fresh = Arc::new(TokenBudget::new(250));
        let mut config = AgentConfig::new(20);
        config.token_budget = Some(Arc::clone(&fresh));
        run(&chat, AgentTools::Full(&tb), &config)
            .await
            .unwrap_err();
        assert_eq!(fresh.used(), 0);
    }

    #[tokio::test]
    async fn provider_meta_is_kept_and_resent() {
        let tb = default_toolbox();
        let mut first = tools_turn(vec![read("c1", "src/a.rs")]).unwrap();
        first.provider_meta = Some(r#"{"reasoning_content":"t"}"#.into());
        let chat = ScriptedChat::new(vec![Ok(first), tools_turn(vec![submit("c2", "done")])]);
        let out = run(&chat, AgentTools::Full(&tb), &AgentConfig::new(20))
            .await
            .unwrap();
        assert_eq!(out.result, "done");
        let reqs = chat.requests();
        assert_eq!(
            reqs[1].messages[1],
            Message::Assistant {
                parts: vec![
                    AssistantPart::ProviderMeta(r#"{"reasoning_content":"t"}"#.into()),
                    AssistantPart::ToolCall(read("c1", "src/a.rs")),
                ],
            }
        );
        assert_eq!(
            message_char_len(&reqs[1].messages[1]),
            message_char_len(&assistant_calls(&["c1"]))
                + r#"{"reasoning_content":"t"}"#.chars().count()
        );
    }
}
