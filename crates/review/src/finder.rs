//! First pass of the review: Finder agents look for defects in each diff batch. Batches run
//! [`BATCH_CONCURRENCY`] at a time and the specialists of one batch run in parallel. Every
//! agent must finish by the Finder deadline: it wraps up (submits) when the deadline gets
//! close, and one still running at the deadline counts as a failed task.
//! When the model rejects tool calling, the whole Finder is run again without tools.
//! A batch that would start once the review's token budget is exhausted is not started.

use std::sync::Arc;

use adus_core::types::ReviewMode;
use adus_db::memories::Memory;
use adus_git::PullRequest;
use adus_llm::ReasoningEffort;
use adus_llm::ToolDef;
use adus_llm::router::{ChatService, UsageContext};
use futures::{StreamExt, future, stream};
use serde_json::Value;
use tokio::time::Instant;
use uuid::Uuid;

use crate::agent::run_react_loop;
use crate::agent::{AgentConfig, AgentError, AgentLlm, AgentOutcome, AgentTools, SubmitSpec};
use crate::budget::TokenBudget;
use crate::parse::{
    Suggestion, parse_submit_findings, parse_suggestions, submit_findings_tool_def,
};
use crate::prompt::{DiffBatch, build_finder_system_prompt, build_finder_user_prompt};
use crate::specialists::{SpecialistKind, active_specialists};
use crate::tools::Toolbox;

/// Diff batches reviewed at the same time (spec 4.7).
pub const BATCH_CONCURRENCY: usize = 2;
/// LLM calls one Finder agent may make.
pub const FINDER_MAX_STEPS: usize = 20;
/// The last Finder steps that offer only `submit_findings`.
pub const FINDER_WRAP_UP_STEPS: usize = 2;

/// Where the Finder talks to the model: the provider's `primary_model` at `finder_effort(mode)`.
#[derive(Clone, Copy)]
pub struct FinderLlm<'a> {
    pub chat: &'a dyn ChatService,
    pub provider_id: Uuid,
    pub model: &'a str,
    pub effort: ReasoningEffort,
    pub usage: &'a UsageContext,
    /// The review's shared input-token budget, if any.
    pub token_budget: Option<&'a Arc<TokenBudget>>,
}

/// What every Finder agent of one review is shown.
pub struct FinderInput<'a> {
    pub pr: &'a PullRequest,
    pub manifest: &'a str,
    pub batches: &'a [DiffBatch<'a>],
    pub memories: &'a [Memory],
    pub repo_instructions: Option<&'a str>,
    pub mode: ReviewMode,
}

/// Why a Finder task has no result.
#[derive(Debug)]
pub enum FinderFailure {
    /// The Finder deadline passed first.
    TimedOut,
    /// The batch was not started: the review's token budget was already exhausted.
    TokenBudget,
    Agent(AgentError),
}

/// One Finder agent: a specialist on one batch.
#[derive(Debug)]
pub struct FinderTask {
    /// Index into `FinderInput::batches`.
    pub batch: usize,
    pub kind: SpecialistKind,
    pub result: Result<AgentOutcome<Vec<Suggestion>>, FinderFailure>,
}

#[derive(Debug)]
pub struct FinderRun {
    /// Ordered by (batch index, specialist order).
    pub tasks: Vec<FinderTask>,
    /// The model rejected tool calling, so `tasks` come from the re-run without tools.
    pub tools_unsupported: bool,
}

/// Runs every Finder agent. With a `toolbox` the agents get the repository tools; without
/// one they review the diff only but may still submit through `submit_findings`. If any
/// agent reports `ToolsUnsupported`, the whole tool-mode result is discarded and every agent
/// runs again with no tools at all, under the same `deadline`.
pub async fn run_finder(
    llm: FinderLlm<'_>,
    input: &FinderInput<'_>,
    toolbox: Option<&Toolbox>,
    deadline: Instant,
) -> FinderRun {
    let tools = toolbox.map_or(AgentTools::SubmitOnly, AgentTools::Full);
    let tasks = run_all(llm, input, tools, deadline).await;
    let rejected = tasks.iter().any(|task| {
        matches!(
            task.result,
            Err(FinderFailure::Agent(AgentError::ToolsUnsupported(_)))
        )
    });
    if !rejected {
        return FinderRun {
            tasks,
            tools_unsupported: false,
        };
    }
    tracing::warn!("finder model rejected tool calling; re-running the finder diff-only");
    FinderRun {
        tasks: run_all(llm, input, AgentTools::NoTools, deadline).await,
        tools_unsupported: true,
    }
}

async fn run_all(
    llm: FinderLlm<'_>,
    input: &FinderInput<'_>,
    tools: AgentTools<'_>,
    deadline: Instant,
) -> Vec<FinderTask> {
    let with_tools = matches!(tools, AgentTools::Full(_));
    let submit = SubmitFindings::new();
    // Futures are built up front: a closure inside the stream would make the future lose
    // `Send` (higher-ranked lifetime inference), as in `agent.rs`.
    let batches: Vec<_> = input
        .batches
        .iter()
        .enumerate()
        .map(|(index, batch)| {
            let prompt = build_finder_user_prompt(
                input.pr,
                input.manifest,
                batch,
                input.memories,
                input.repo_instructions,
                with_tools,
            );
            let kinds = active_specialists(input.mode, !batch.rules.is_empty());
            let submit = &submit;
            async move {
                // Checked when the batch starts (the stream polls it), not when it is built.
                if llm.token_budget.is_some_and(|budget| budget.exhausted()) {
                    tracing::debug!(batch = index, "token budget exhausted; batch not started");
                    return kinds
                        .into_iter()
                        .map(|kind| FinderTask {
                            batch: index,
                            kind,
                            result: Err(FinderFailure::TokenBudget),
                        })
                        .collect::<Vec<_>>();
                }
                let agents: Vec<_> = kinds
                    .into_iter()
                    .map(|kind| run_one(llm, kind, prompt.clone(), tools, submit, deadline))
                    .collect();
                let results = future::join_all(agents).await;
                results
                    .into_iter()
                    .map(|(kind, result)| FinderTask {
                        batch: index,
                        kind,
                        result,
                    })
                    .collect::<Vec<_>>()
            }
        })
        .collect();
    let per_batch: Vec<Vec<FinderTask>> = stream::iter(batches)
        // `buffered` keeps the batch order, so findings stay in a stable order.
        .buffered(BATCH_CONCURRENCY)
        .collect()
        .await;
    per_batch.into_iter().flatten().collect()
}

/// One specialist on one batch, cut at `deadline`.
async fn run_one(
    llm: FinderLlm<'_>,
    kind: SpecialistKind,
    prompt: String,
    tools: AgentTools<'_>,
    submit: &SubmitFindings,
    deadline: Instant,
) -> (
    SpecialistKind,
    Result<AgentOutcome<Vec<Suggestion>>, FinderFailure>,
) {
    let system = build_finder_system_prompt(kind, matches!(tools, AgentTools::Full(_)));
    let agent = AgentLlm {
        chat: llm.chat,
        provider_id: llm.provider_id,
        model: llm.model,
        system: &system,
        effort: Some(llm.effort),
        usage: llm.usage,
    };
    // The agent wraps up on its own before the deadline; `timeout_at` stays the hard cut.
    let config = AgentConfig {
        wrap_up_steps: FINDER_WRAP_UP_STEPS,
        deadline: Some(deadline),
        token_budget: llm.token_budget.cloned(),
        ..AgentConfig::new(FINDER_MAX_STEPS)
    };
    let run = run_react_loop(&agent, tools, submit, &config, prompt);
    let result = match tokio::time::timeout_at(deadline, run).await {
        Ok(Ok(outcome)) => {
            tracing::debug!(
                specialist = kind.label(),
                steps_used = outcome.steps_used,
                candidates = outcome.result.len(),
                cut_short = outcome.cut_short,
                "finder agent finished"
            );
            Ok(outcome)
        }
        Ok(Err(err)) => {
            tracing::warn!(specialist = kind.label(), error = %err, "finder agent failed");
            Err(FinderFailure::Agent(err))
        }
        Err(_elapsed) => {
            tracing::warn!(
                specialist = kind.label(),
                "finder agent cut by the deadline"
            );
            Err(FinderFailure::TimedOut)
        }
    };
    (kind, result)
}

/// The Finder's terminal tool, `submit_findings`. A reply without a tool call is read as the
/// JSON array of the diff-only output format.
struct SubmitFindings {
    def: ToolDef,
}

impl SubmitFindings {
    fn new() -> Self {
        Self {
            def: submit_findings_tool_def(),
        }
    }
}

impl SubmitSpec for SubmitFindings {
    type Output = Vec<Suggestion>;

    fn tool_def(&self) -> &ToolDef {
        &self.def
    }

    fn parse_arguments(&self, arguments: &Value) -> Result<Vec<Suggestion>, String> {
        parse_submit_findings(arguments)
    }

    fn parse_prose(&self, text: &str) -> Result<Vec<Suggestion>, String> {
        parse_suggestions(text).map_err(|err| err.to_string())
    }
}
