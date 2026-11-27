//! First pass of the review: Finder agents look for defects in each diff batch. Batches run
//! [`BATCH_CONCURRENCY`] at a time and the specialists of one batch run in parallel. Every
//! agent must finish by the Finder deadline: it wraps up (submits) when the deadline gets
//! close, and one still running at the deadline counts as a failed task.
//! When the model rejects tool calling, the whole Finder is run again without tools.
//! A batch that would start once the review's token budget is exhausted is not started.
//!
//! A pass that submits no findings although its review log reads like it found some gets one
//! prose recovery call on the fast model. With the repository tools, `Generalist` and `Bug`
//! then run a second pass, the synthesis rescue, when enough budget and time are left: it
//! looks for defects the first pass missed, and its new findings are merged in.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use adus_core::types::{ReviewMode, normalize_path};
use adus_db::memories::Memory;
use adus_git::{FileDiff, PullRequest};
use adus_llm::ReasoningEffort;
use adus_llm::ToolDef;
use adus_llm::router::ChatError;
use futures::{StreamExt, future, stream};
use serde_json::Value;
use tokio::time::Instant;
use tracing::Instrument;

use crate::agent::{
    AgentConfig, AgentError, AgentOutcome, AgentTools, LlmContext, SubmitSpec, run_react_loop,
};
use crate::budget::TokenBudget;
use crate::coverage::CoverageLedger;
use crate::engine::{EngineError, chat_error, is_retryable, timed_out};
use crate::parse::{
    CommentableRange, Suggestion, dedup_exact, keep_valid_batch, parse_dismissed, parse_review_log,
    parse_submit_findings, parse_suggestions, submit_findings_tool_def, validate_batch_anchors,
};
use crate::prompt::{
    DiffBatch, SkipReason, build_finder_system_prompt, build_finder_user_prompt, or_none,
    truncate_with_marker, wrap,
};
use crate::redact::redact_secrets;
use crate::specialists::{SpecialistKind, active_specialists};
use crate::tools::Toolbox;
use crate::verifier::CandidateFinding;

/// Diff batches reviewed at the same time (SPEC.md §8.4).
const BATCH_CONCURRENCY: usize = 2;
/// LLM calls a Finder agent may make on a batch of up to `FINDER_FREE_FILES` files.
const FINDER_BASE_STEPS: usize = 20;
/// Files per batch covered by `FINDER_BASE_STEPS`; each two files beyond add one step.
const FINDER_FREE_FILES: usize = 8;
/// Most LLM calls one Finder agent may make, however large the batch.
const FINDER_STEP_CAP: usize = 30;
/// The last Finder steps that offer only `submit_findings`.
const FINDER_WRAP_UP_STEPS: usize = 2;
/// Most LLM calls of the synthesis rescue (Pass 2).
const SYNTHESIS_MAX_STEPS: usize = 12;
/// Pass 2 runs only while at least this percentage of the weighted token budget is left.
const SYNTHESIS_MIN_BUDGET_SHARE: u32 = 25;
/// Pass 2 needs at least this many Pass 1 steps' worth of time before the Finder deadline...
const SYNTHESIS_WINDOW_STEPS: u32 = 6;
/// ...and never less than this.
const SYNTHESIS_MIN_WINDOW: Duration = Duration::from_secs(60);
/// The Pass 1 step time assumed when Pass 1 measured none.
const SYNTHESIS_DEFAULT_STEP: Duration = Duration::from_secs(90);
/// Characters of a finding's body shown in `<current_findings>`.
const FINDING_EXCERPT_CHARS: usize = 200;
/// Characters of the normalized body that, with file and line, identify a finding when the
/// passes are merged.
const MERGE_BODY_PREFIX_CHARS: usize = 80;
/// Shortest review log or reply text that may hold findings written as prose.
const PROSE_MIN_CHARS: usize = 80;
/// Signal groups (defect words, action words, file/line references) prose must match.
const PROSE_MIN_SIGNAL_GROUPS: usize = 2;
/// Output cap of the prose recovery call.
const PROSE_RECOVERY_MAX_TOKENS: u32 = 2_048;
/// Prose recovery runs on the fast model at this effort (spec §2).
const PROSE_RECOVERY_EFFORT: ReasoningEffort = ReasoningEffort::Medium;
/// Word stems that name a defect.
const DEFECT_STEMS: &[&str] = &[
    "bug",
    "crash",
    "panic",
    "error",
    "fail",
    "broken",
    "wrong",
    "incorrect",
    "leak",
    "race",
    "overflow",
    "deadlock",
    "vulnerab",
    "inject",
    "unsafe",
    "null",
    "missing",
    "regress",
];
/// Words that ask for a change.
const ACTION_WORDS: &[&str] = &[
    "should", "must", "fix", "instead", "replace", "ensure", "avoid", "guard", "handle", "validate",
];

const SYNTHESIS_TASK: &str = "Second look. You already reviewed this change once: <already_inspected_files> lists the files you opened, <recent_investigation> your latest tool calls, and <current_findings> what you reported. Look again for concrete defects that review missed: error and failure paths, what callers pass in and expect back, edge cases and boundary values, and branches the change reaches that you did not follow. Open whatever code you need with the tools. Do not report a finding from <current_findings> again, reworded or moved to another line. Call submit_findings with only the new findings, or with an empty findings array when you find none.";

const PROSE_RECOVERY_SYSTEM: &str = "You turn a code reviewer's free-text notes into structured findings. You do not review code yourself, and you never add a defect the notes do not state.";

const PROSE_RECOVERY_TASK: &str = "The notes in <reviewer_notes> describe defects in prose instead of submitting them. Extract every concrete defect the notes report as real; leave out suspicions they rule out or call safe. Answer with only a JSON array. Each element is an object with `file` (one of the paths in <batch_files>, written exactly as there), `line` (the new-file line number the notes give), `category` (\"bug\", \"security\", \"performance\" or \"repo_instruction\") and `body` (what triggers the defect, what goes wrong and where, in the notes' own terms). Skip a defect whose file is not in <batch_files> or whose line the notes do not give. Answer [] when the notes report no defect.";

/// LLM calls one Finder agent may make on a batch of `files` files.
fn finder_max_steps(files: usize) -> usize {
    (FINDER_BASE_STEPS + files.saturating_sub(FINDER_FREE_FILES).div_ceil(2)).min(FINDER_STEP_CAP)
}

/// Where the Finder talks to the model: both passes use `primary` at `effort`; prose recovery
/// uses `fast_model` at [`PROSE_RECOVERY_EFFORT`].
#[derive(Clone, Copy)]
pub(crate) struct FinderLlm<'a> {
    pub primary: LlmContext<'a>,
    pub fast_model: &'a str,
    pub effort: ReasoningEffort,
}

/// What every Finder agent of one review is shown.
pub(crate) struct FinderInput<'a> {
    pub pr: &'a PullRequest,
    pub manifest: &'a str,
    pub batches: &'a [DiffBatch<'a>],
    pub allowed: &'a HashMap<String, CommentableRange>,
    pub memories: &'a [Memory],
    pub repo_instructions: Option<&'a str>,
    pub mode: ReviewMode,
}

/// Commentable ranges for the files in one `DiffBatch`, keyed by normalized path.
pub(crate) fn batch_allowed<'a>(
    batch: &DiffBatch<'_>,
    allowed: &'a HashMap<String, CommentableRange>,
) -> HashMap<String, &'a CommentableRange> {
    batch
        .files
        .iter()
        .filter_map(|file| {
            let path = normalize_path(&file.path);
            let range = allowed.get(&path)?;
            Some((path, range))
        })
        .collect()
}

/// Which pass produced a finding: one of the Finder's, or the Sharded Rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FinderPass {
    Pass1,
    /// The second, `synthesis-rescue` pass.
    SynthesisRescue,
    /// Recovered from a pass's prose by the fast model.
    ProseRecovery,
    /// A team-rule violation the Sharded Rules verified with evidence (`rules_judge.rs`); it
    /// skips the Verifier.
    ShardedRules,
}

impl FinderPass {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Pass1 => "pass1",
            Self::SynthesisRescue => "synthesis_rescue",
            Self::ProseRecovery => "prose_recovery",
            Self::ShardedRules => "sharded_rules",
        }
    }
}

/// Why a `Generalist` or `Bug` agent with the repository tools did not run Pass 2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SynthesisSkipped {
    /// Less than [`SYNTHESIS_MIN_BUDGET_SHARE`] % of the token budget was left.
    Budget,
    /// The Finder deadline left less than the synthesis window.
    Time,
}

impl SynthesisSkipped {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Budget => "budget",
            Self::Time => "time",
        }
    }
}

/// A finding and the pass that produced it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct FinderFinding {
    pub suggestion: Suggestion,
    pub origin: FinderPass,
}

/// Why a Finder task has no result.
#[derive(Debug)]
pub(crate) enum FinderFailure {
    /// The Finder deadline passed first.
    TimedOut,
    /// The batch was not started: the review's token budget was already exhausted.
    TokenBudget,
    Agent(AgentError),
}

/// One Finder agent: a specialist on one batch, both passes.
#[derive(Debug)]
pub(crate) struct FinderTask {
    /// Index into `FinderInput::batches`.
    pub batch: usize,
    pub kind: SpecialistKind,
    /// Pass 1's outcome with the merged findings of every pass, Pass 2's steps added, and the
    /// files either pass opened. A failed Pass 2 only loses its own part.
    pub result: Result<AgentOutcome<Vec<FinderFinding>>, FinderFailure>,
    pub synthesis_skipped: Option<SynthesisSkipped>,
    /// Findings Pass 2 added after the merge.
    pub synthesis_candidates: usize,
}

impl FinderTask {
    fn failed(batch: usize, kind: SpecialistKind, failure: FinderFailure) -> Self {
        Self {
            batch,
            kind,
            result: Err(failure),
            synthesis_skipped: None,
            synthesis_candidates: 0,
        }
    }
}

#[derive(Debug)]
pub(crate) struct FinderRun {
    /// Ordered by (batch index, specialist order).
    pub tasks: Vec<FinderTask>,
    /// The model rejected tool calling, so `tasks` come from the re-run without tools.
    pub tools_unsupported: bool,
    /// The first task (in `tasks` order) that skipped Pass 2, and why.
    pub synthesis_skipped: Option<SynthesisSkipped>,
    /// Findings every Pass 2 added, summed.
    pub synthesis_candidates: usize,
}

impl FinderRun {
    fn new(tasks: Vec<FinderTask>, tools_unsupported: bool) -> Self {
        Self {
            synthesis_skipped: tasks.iter().find_map(|task| task.synthesis_skipped),
            synthesis_candidates: tasks.iter().map(|task| task.synthesis_candidates).sum(),
            tasks,
            tools_unsupported,
        }
    }
}

/// Runs every Finder agent. With a `toolbox` the agents get the repository tools; without
/// one they review the diff only but may still submit through `submit_findings`. If any
/// agent reports `ToolsUnsupported`, the whole tool-mode result is discarded and every agent
/// runs again with no tools at all, under the same `deadline`.
pub(crate) async fn run_finder(
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
        return FinderRun::new(tasks, false);
    }
    tracing::warn!("finder model rejected tool calling; re-running the finder diff-only");
    FinderRun::new(
        run_all(llm, input, AgentTools::NoTools, deadline).await,
        true,
    )
}

/// What the Finder stage leaves for the stage log and the budget warning.
pub(crate) struct FinderStats {
    /// The Finder re-ran diff-only because the primary model rejected tool calling.
    pub tools_unsupported: bool,
    /// [`TokenBudget::wrap_ups`] read when the Finder finished.
    pub wrap_ups: usize,
    /// Batches never started because the token budget was exhausted.
    pub budget_skipped_batches: usize,
    pub tasks: usize,
    pub steps: usize,
    pub gate_refusals: usize,
    pub synthesis_skipped: Option<&'static str>,
    pub synthesis_candidates: usize,
}

/// Findings per origin, as `pass1=N synthesis_rescue=N prose_recovery=N sharded_rules=N`.
pub(crate) fn count_by_origin(candidates: &[CandidateFinding]) -> String {
    [
        FinderPass::Pass1,
        FinderPass::SynthesisRescue,
        FinderPass::ProseRecovery,
        FinderPass::ShardedRules,
    ]
    .iter()
    .map(|&pass| {
        let count = candidates.iter().filter(|c| c.origin == pass).count();
        format!("{}={count}", pass.label())
    })
    .collect::<Vec<_>>()
    .join(" ")
}

/// LLM calls and completion-gate refusals summed over the Finder tasks that finished (both
/// passes).
pub(crate) fn finder_steps_and_refusals(tasks: &[FinderTask]) -> (usize, usize) {
    tasks
        .iter()
        .filter_map(|task| task.result.as_ref().ok())
        .fold((0, 0), |(steps, refusals), outcome| {
            (
                steps + outcome.steps_used,
                refusals + usize::from(outcome.gate_refused),
            )
        })
}

/// Turns Finder results into candidates for the Verifier, in (batch, specialist) order: each
/// result is filtered to its batch's [`CommentableRange`]s (commentable lines, and hunks for a
/// `start_line`), then exact `(file, line, title)` duplicates are merged across the whole
/// review (see [`dedup_exact`]).
///
/// A failed task becomes a summary note when another agent reviewed its batch; when no agent
/// reviewed a batch, its files are listed as skipped (reason `timed_out` when the Finder
/// deadline cut every agent of it, `token_budget` when the batch was never started because
/// the token budget was exhausted). When no task succeeded at all, the review fails (see
/// [`all_failed`]); in `Generalist` mode it is also retried when every failure was retryable
/// (see [`generalist_retry`]).
pub(crate) fn collect_candidates(
    tasks: Vec<FinderTask>,
    mode: ReviewMode,
    batches: &[DiffBatch<'_>],
    allowed: &HashMap<String, CommentableRange>,
    skipped: &mut Vec<(String, SkipReason)>,
    warnings: &mut Vec<String>,
) -> Result<(Vec<CandidateFinding>, usize), EngineError> {
    if !tasks.is_empty() && tasks.iter().all(|task| task.result.is_err()) {
        return Err(all_failed(tasks));
    }
    if mode == ReviewMode::Generalist
        && let Some(err) = generalist_retry(&tasks)
    {
        return Err(err);
    }
    let reviewed: HashSet<usize> = tasks
        .iter()
        .filter(|task| task.result.is_ok())
        .map(|task| task.batch)
        .collect();
    let batch_ranges: Vec<HashMap<String, &CommentableRange>> = batches
        .iter()
        .map(|batch| batch_allowed(batch, allowed))
        .collect();

    let mut candidates = Vec::new();
    let mut filtered_dropped = 0usize;
    // Batch index → skip reason, for batches no agent reviewed.
    let mut unreviewed: BTreeMap<usize, SkipReason> = BTreeMap::new();
    for task in tasks {
        let label = task.kind.label();
        let reason = match task.result {
            Ok(outcome) => {
                let Some(range) = batch_ranges.get(task.batch) else {
                    continue;
                };
                let AgentOutcome {
                    result,
                    opened_files,
                    ..
                } = outcome;
                let raw_count = result.len();
                let found = result
                    .into_iter()
                    .map(|finding| CandidateFinding {
                        investigated_by_finder: opened_files
                            .contains(&normalize_path(&finding.suggestion.file)),
                        origin: finding.origin,
                        suggestion: finding.suggestion,
                        specialist: task.kind,
                    })
                    .collect();
                let valid = keep_valid_batch(found, range);
                filtered_dropped += raw_count.saturating_sub(valid.len());
                candidates.extend(valid);
                continue;
            }
            Err(FinderFailure::TimedOut) => {
                push_once(warnings, format!("{label} reviewer did not finish in time"));
                SkipReason::TimedOut
            }
            // The summary's token-budget note covers it; no per-reviewer note.
            Err(FinderFailure::TokenBudget) => SkipReason::TokenBudget,
            Err(FinderFailure::Agent(_)) if reviewed.contains(&task.batch) => {
                push_once(warnings, format!("{label} reviewer did not complete"));
                continue;
            }
            Err(FinderFailure::Agent(AgentError::Unparseable(_))) => SkipReason::Unparseable,
            Err(FinderFailure::Agent(_)) => SkipReason::LlmError,
        };
        if reviewed.contains(&task.batch) {
            continue;
        }
        // Differing reasons for one batch read as a plain LLM error.
        unreviewed
            .entry(task.batch)
            .and_modify(|current| {
                if *current != reason {
                    *current = SkipReason::LlmError;
                }
            })
            .or_insert(reason);
    }
    for (index, reason) in unreviewed {
        if let Some(batch) = batches.get(index) {
            tracing::warn!(
                batch = index,
                reason = reason.as_str(),
                "no finder agent reviewed a batch"
            );
            skipped.extend(batch.files.iter().map(|file| (file.path.clone(), reason)));
        }
    }
    Ok((dedup_exact(candidates, |c| &c.suggestion), filtered_dropped))
}

/// Generalist rule of spec decision 8: when some Finder task failed and every failure was a
/// retryable chat error (`AllKeysFailed`, `NoKeys`, `Internal`), retry the review rather than
/// post it with those files skipped. A deadline cut is never retryable. The retry waits for
/// the first `AllKeysFailed`'s `retry_after`.
fn generalist_retry(tasks: &[FinderTask]) -> Option<EngineError> {
    let mut first_retryable: Option<&ChatError> = None;
    let mut rate_limited: Option<&ChatError> = None;
    for task in tasks {
        let err = match &task.result {
            Ok(_) => continue,
            Err(FinderFailure::Agent(AgentError::Chat(err))) if is_retryable(err) => err,
            Err(_) => return None,
        };
        first_retryable.get_or_insert(err);
        if matches!(err, ChatError::AllKeysFailed { .. }) {
            rate_limited.get_or_insert(err);
        }
    }
    rate_limited.or(first_retryable).cloned().map(chat_error)
}

/// The error when no Finder task succeeded. Retryable only when every failure was a
/// retryable chat error (`AllKeysFailed`, `NoKeys`, `Internal`); otherwise the first other
/// failure decides, and a review cut entirely by the deadline is `review timed out`.
fn all_failed(tasks: Vec<FinderTask>) -> EngineError {
    let mut retryable: Option<ChatError> = None;
    let mut fatal: Option<EngineError> = None;
    for task in tasks {
        let error = match task.result {
            Ok(_) => continue,
            Err(FinderFailure::TimedOut) => timed_out(),
            // Never retryable: a retry would spend the same tokens again.
            Err(FinderFailure::TokenBudget) => {
                EngineError::Fatal("review stopped: token budget reached".into())
            }
            Err(FinderFailure::Agent(AgentError::Chat(err))) if is_retryable(&err) => {
                retryable.get_or_insert(err);
                continue;
            }
            Err(FinderFailure::Agent(AgentError::Chat(err))) => chat_error(err),
            Err(FinderFailure::Agent(AgentError::Unparseable(_))) => {
                EngineError::Fatal("LLM: review output could not be parsed".into())
            }
            Err(FinderFailure::Agent(AgentError::ToolsUnsupported(message))) => {
                chat_error(ChatError::ToolsUnsupported(message))
            }
        };
        fatal.get_or_insert(error);
    }
    match (fatal, retryable) {
        (Some(error), _) => error,
        (None, Some(err)) => chat_error(err),
        (None, None) => timed_out(),
    }
}

fn push_once(warnings: &mut Vec<String>, warning: String) {
    if !warnings.contains(&warning) {
        warnings.push(warning);
    }
}

async fn run_all(
    llm: FinderLlm<'_>,
    input: &FinderInput<'_>,
    tools: AgentTools<'_>,
    deadline: Instant,
) -> Vec<FinderTask> {
    let with_tools = matches!(tools, AgentTools::Full(_));
    // Futures are built up front: a closure inside the stream would make the future lose
    // `Send` (higher-ranked lifetime inference), as in `agent.rs`.
    let batches: Vec<_> = input
        .batches
        .iter()
        .enumerate()
        .map(|(index, batch)| {
            let kinds = active_specialists(input.mode);
            let allowed = batch_allowed(batch, input.allowed);
            async move {
                // Checked when the batch starts (the stream polls it), not when it is built.
                if llm
                    .primary
                    .token_budget
                    .is_some_and(|budget| budget.exhausted())
                {
                    tracing::debug!(batch = index, "token budget exhausted; batch not started");
                    return kinds
                        .into_iter()
                        .map(|kind| FinderTask::failed(index, kind, FinderFailure::TokenBudget))
                        .collect::<Vec<_>>();
                }
                let agents: Vec<_> = kinds
                    .into_iter()
                    .map(|kind| {
                        let prompt = build_finder_user_prompt(
                            input.pr,
                            input.manifest,
                            batch,
                            input.memories,
                            input.repo_instructions,
                            kind,
                            with_tools,
                        );
                        let job = FinderJob {
                            kind,
                            prompt,
                            files: &batch.files,
                            max_steps: finder_max_steps(batch.files.len()),
                            rescue: false,
                        };
                        run_task(llm, job, tools, allowed.clone(), index, deadline)
                    })
                    .collect();
                future::join_all(agents).await
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

/// What one Finder pass works on.
struct FinderJob<'a> {
    kind: SpecialistKind,
    prompt: String,
    /// The batch's files: the hunks a gated run must read.
    files: &'a [&'a FileDiff],
    max_steps: usize,
    /// The synthesis rescue: never gated, no coverage ledger.
    rescue: bool,
}

/// One specialist on one batch: Pass 1, prose recovery when Pass 1 wrote its findings as
/// prose, then (`Generalist` and `Bug` with the repository tools, when [`synthesis_gate`]
/// allows) Pass 2 with its own prose recovery, merged into Pass 1's findings.
async fn run_task(
    llm: FinderLlm<'_>,
    job: FinderJob<'_>,
    tools: AgentTools<'_>,
    allowed: HashMap<String, &CommentableRange>,
    batch: usize,
    deadline: Instant,
) -> FinderTask {
    let kind = job.kind;
    let rescue_job = FinderJob {
        kind,
        prompt: String::new(),
        files: job.files,
        max_steps: job.max_steps.min(SYNTHESIS_MAX_STEPS),
        rescue: true,
    };
    let prompt = job.prompt.clone();
    let submit = SubmitFindings::new(kind, batch, allowed.clone());
    let mut pass1 = match run_one(llm, job, tools, &submit, deadline).await {
        Ok(outcome) => outcome,
        Err(failure) => return FinderTask::failed(batch, kind, failure),
    };
    let submission = std::mem::take(&mut pass1.result);
    let mut findings = pass_findings(llm, &submit, submission, FinderPass::Pass1, deadline).await;
    let mut outcome = pass1.map(|_| Vec::new());

    let mut synthesis_skipped = None;
    let mut synthesis_candidates = 0;
    let rescues = matches!(tools, AgentTools::Full(_))
        && matches!(kind, SpecialistKind::Generalist | SpecialistKind::Bug);
    if rescues {
        let gate = synthesis_gate(
            llm.primary.token_budget.map(Arc::as_ref),
            deadline.saturating_duration_since(Instant::now()),
            outcome.mean_step,
        );
        match gate {
            Err(skipped) => {
                tracing::debug!(
                    specialist = kind.label(),
                    batch,
                    synthesis_skipped = skipped.label(),
                    "synthesis rescue skipped"
                );
                synthesis_skipped = Some(skipped);
            }
            Ok(()) => {
                let rescue_job = FinderJob {
                    prompt: synthesis_prompt(&prompt, &outcome, &findings),
                    ..rescue_job
                };
                let submit = SubmitFindings::new(kind, batch, allowed);
                match run_one(llm, rescue_job, tools, &submit, deadline).await {
                    Ok(mut pass2) => {
                        let submission = std::mem::take(&mut pass2.result);
                        let added = pass_findings(
                            llm,
                            &submit,
                            submission,
                            FinderPass::SynthesisRescue,
                            deadline,
                        )
                        .await;
                        synthesis_candidates = merge_findings(&mut findings, added);
                        outcome.steps_used += pass2.steps_used;
                        outcome.opened_files.extend(pass2.opened_files);
                    }
                    // Pass 1's findings stand on their own.
                    Err(failure) => tracing::warn!(
                        specialist = kind.label(),
                        batch,
                        failure = ?failure,
                        "synthesis rescue failed; keeping the first pass findings"
                    ),
                }
            }
        }
    }
    outcome.result = findings;
    FinderTask {
        batch,
        kind,
        result: Ok(outcome),
        synthesis_skipped,
        synthesis_candidates,
    }
}

/// Whether Pass 2 may start: at least [`SYNTHESIS_MIN_BUDGET_SHARE`] % of the weighted token
/// budget is left (always, without a budget), and `time_left` to the Finder deadline is at
/// least [`SYNTHESIS_WINDOW_STEPS`] × Pass 1's mean step ([`SYNTHESIS_DEFAULT_STEP`] when it
/// measured none), but never less than [`SYNTHESIS_MIN_WINDOW`].
fn synthesis_gate(
    budget: Option<&TokenBudget>,
    time_left: Duration,
    pass1_mean_step: Option<Duration>,
) -> Result<(), SynthesisSkipped> {
    if budget.is_some_and(|budget| budget.remaining_share_percent() < SYNTHESIS_MIN_BUDGET_SHARE) {
        return Err(SynthesisSkipped::Budget);
    }
    let window = pass1_mean_step
        .unwrap_or(SYNTHESIS_DEFAULT_STEP)
        .saturating_mul(SYNTHESIS_WINDOW_STEPS)
        .max(SYNTHESIS_MIN_WINDOW);
    if time_left < window {
        return Err(SynthesisSkipped::Time);
    }
    Ok(())
}

/// Pass 1's prompt unchanged (so the provider's prompt cache still matches it), then what
/// Pass 1 opened, its latest tool calls and its findings, then the rescue task.
fn synthesis_prompt(
    prompt: &str,
    pass1: &AgentOutcome<Vec<FinderFinding>>,
    findings: &[FinderFinding],
) -> String {
    let mut opened: Vec<&str> = pass1.opened_files.iter().map(String::as_str).collect();
    opened.sort_unstable();
    let current = findings
        .iter()
        .map(|finding| {
            let s = &finding.suggestion;
            let body = s.body.split_whitespace().collect::<Vec<_>>().join(" ");
            let excerpt = truncate_with_marker(&body, FINDING_EXCERPT_CHARS, "...");
            format!("- {}:{}: {excerpt}", s.file, s.line)
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "{prompt}\n\n{}\n{}\n{}\n\n{SYNTHESIS_TASK}",
        wrap("already_inspected_files", &or_none(opened.join("\n"))),
        wrap(
            "recent_investigation",
            &or_none(pass1.recent_tool_calls.join("\n"))
        ),
        wrap("current_findings", &or_none(current)),
    )
}

/// Appends the findings of `added` that no finding of `findings` already has (same normalized
/// file, line and normalized body prefix); returns how many were appended.
fn merge_findings(findings: &mut Vec<FinderFinding>, added: Vec<FinderFinding>) -> usize {
    let mut seen: HashSet<(String, u32, String)> = findings
        .iter()
        .map(|finding| merge_key(&finding.suggestion))
        .collect();
    let before = findings.len();
    findings.extend(
        added
            .into_iter()
            .filter(|finding| seen.insert(merge_key(&finding.suggestion))),
    );
    findings.len() - before
}

fn merge_key(s: &Suggestion) -> (String, u32, String) {
    let body = s
        .body
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    let prefix = body.chars().take(MERGE_BODY_PREFIX_CHARS).collect();
    (normalize_path(&s.file), s.line, prefix)
}

/// The findings of one pass, tagged `origin`. A pass that submitted none but whose prose
/// [`looks_like_findings`] gets one [`recover_prose_findings`] call.
async fn pass_findings(
    llm: FinderLlm<'_>,
    submit: &SubmitFindings<'_>,
    submission: FinderSubmission,
    origin: FinderPass,
    deadline: Instant,
) -> Vec<FinderFinding> {
    let tag = |found: Vec<Suggestion>, origin| {
        found
            .into_iter()
            .map(|suggestion| FinderFinding { suggestion, origin })
            .collect()
    };
    if !submission.findings.is_empty() {
        return tag(submission.findings, origin);
    }
    let Some(prose) = submission.prose.filter(|prose| looks_like_findings(prose)) else {
        return Vec::new();
    };
    let recovered = recover_prose_findings(llm, submit, &prose, deadline).await;
    tracing::debug!(
        specialist = submit.kind.label(),
        batch = submit.batch,
        pass = origin.label(),
        recovered = recovered.len(),
        "finder prose recovery finished"
    );
    tag(recovered, FinderPass::ProseRecovery)
}

/// Whether `text` may describe findings in prose: at least [`PROSE_MIN_CHARS`] characters
/// that match at least [`PROSE_MIN_SIGNAL_GROUPS`] of defect words, action words and a
/// file/line reference (`src/a.rs:12`, `line 12`).
fn looks_like_findings(text: &str) -> bool {
    if text.trim().chars().count() < PROSE_MIN_CHARS {
        return false;
    }
    let lower = text.to_lowercase();
    let words: Vec<&str> = lower
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|word| !word.is_empty())
        .collect();
    let defect = words
        .iter()
        .any(|word| DEFECT_STEMS.iter().any(|stem| word.starts_with(stem)));
    let action = words.iter().any(|word| ACTION_WORDS.contains(word));
    let reference = has_file_line_reference(&lower);
    [defect, action, reference]
        .into_iter()
        .filter(|&signal| signal)
        .count()
        >= PROSE_MIN_SIGNAL_GROUPS
}

/// A `path.ext:12` / `dir/file:12` token, or `line 12` / `lines 12-14`.
fn has_file_line_reference(lower: &str) -> bool {
    let tokens: Vec<&str> = lower
        .split_whitespace()
        .map(|token| token.trim_matches(|c: char| "`'\"()[]{},;".contains(c)))
        .collect();
    let path_line = tokens.iter().any(|token| {
        token.split_once(':').is_some_and(|(path, rest)| {
            !path.is_empty()
                && (path.contains('.') || path.contains('/'))
                && rest.starts_with(|c: char| c.is_ascii_digit())
        })
    });
    let line_word = tokens.windows(2).any(|pair| {
        matches!(pair[0], "line" | "lines") && pair[1].starts_with(|c: char| c.is_ascii_digit())
    });
    path_line || line_word
}

/// One call to the fast model at [`PROSE_RECOVERY_EFFORT`] that turns `prose` into lean
/// findings, keeping only those on a file of the batch. Any failure (chat error, no JSON
/// array, the Finder deadline) recovers nothing.
async fn recover_prose_findings(
    llm: FinderLlm<'_>,
    submit: &SubmitFindings<'_>,
    prose: &str,
    deadline: Instant,
) -> Vec<Suggestion> {
    let mut files: Vec<&str> = submit.allowed.keys().map(String::as_str).collect();
    files.sort_unstable();
    let prompt = format!(
        "{}\n\n{}\n\n{PROSE_RECOVERY_TASK}",
        wrap("batch_files", &files.join("\n")),
        wrap("reviewer_notes", prose),
    );
    let call = llm.primary.with_model(llm.fast_model).single_turn(
        "prose_recovery",
        PROSE_RECOVERY_SYSTEM,
        Some(PROSE_RECOVERY_EFFORT),
        prompt,
        PROSE_RECOVERY_MAX_TOKENS,
    );
    let text = match tokio::time::timeout_at(deadline, call).await {
        Ok(Ok(text)) => text,
        Ok(Err(err)) => {
            tracing::warn!(error = %err, "finder prose recovery failed");
            return Vec::new();
        }
        Err(_elapsed) => {
            tracing::warn!("finder prose recovery cut by the deadline");
            return Vec::new();
        }
    };
    let Ok(found) = parse_suggestions(&text) else {
        tracing::debug!("finder prose recovery answer has no JSON array");
        return Vec::new();
    };
    let in_batch = found
        .into_iter()
        .filter(|s| submit.allowed.contains_key(&normalize_path(&s.file)))
        .collect();
    submit.with_default_category(in_batch)
}

/// One Finder pass of one specialist on one batch, cut at `deadline`. Every pass gets the
/// budget band notes; Pass 1 of `Generalist` and `Bug` with the repository tools also gets a
/// coverage ledger (debt notes) and the completion gate. `Security` and `Performance` look
/// through a narrow lens and may rightly skip a file, so they get neither; nor does the
/// synthesis rescue, which follows a Pass 1 that already read the files.
async fn run_one(
    llm: FinderLlm<'_>,
    job: FinderJob<'_>,
    tools: AgentTools<'_>,
    submit: &SubmitFindings<'_>,
    deadline: Instant,
) -> Result<AgentOutcome<FinderSubmission>, FinderFailure> {
    let FinderJob {
        kind,
        prompt,
        files,
        max_steps,
        rescue,
    } = job;
    let gated = !rescue
        && matches!(tools, AgentTools::Full(_))
        && matches!(kind, SpecialistKind::Generalist | SpecialistKind::Bug);
    let system = build_finder_system_prompt(kind, matches!(tools, AgentTools::Full(_)));
    let agent = llm.primary.for_agent(&system, Some(llm.effort));
    // The agent wraps up on its own before the deadline; `timeout_at` stays the hard cut.
    let config = AgentConfig::finder(
        max_steps,
        FINDER_WRAP_UP_STEPS,
        deadline,
        llm.primary.token_budget,
        gated.then(|| CoverageLedger::from_files(files)),
    );
    let pass = if rescue {
        FinderPass::SynthesisRescue
    } else {
        FinderPass::Pass1
    };
    // Names the agent on every log line of its run (concurrent agents interleave).
    let span = tracing::debug_span!(
        "finder",
        specialist = kind.label(),
        batch = submit.batch,
        pass = pass.label()
    );
    let run = run_react_loop(&agent, tools, submit, &config, prompt).instrument(span);
    match tokio::time::timeout_at(deadline, run).await {
        Ok(Ok(outcome)) => {
            tracing::debug!(
                specialist = kind.label(),
                pass = pass.label(),
                steps_used = outcome.steps_used,
                candidates = outcome.result.findings.len(),
                cut_short = outcome.cut_short,
                gate_refused = outcome.gate_refused,
                "finder agent finished"
            );
            Ok(outcome)
        }
        Ok(Err(err)) => {
            tracing::warn!(specialist = kind.label(), pass = pass.label(), error = %err, "finder agent failed");
            Err(FinderFailure::Agent(err))
        }
        Err(_elapsed) => {
            tracing::warn!(
                specialist = kind.label(),
                pass = pass.label(),
                "finder agent cut by the deadline"
            );
            Err(FinderFailure::TimedOut)
        }
    }
}

/// What one Finder pass submitted: its findings and the prose that may describe more (the
/// `review_log` argument, or the reply text of a prose submission), secret-redacted.
#[derive(Debug, Default)]
struct FinderSubmission {
    findings: Vec<Suggestion>,
    prose: Option<String>,
}

/// The Finder's terminal tool, `submit_findings`. A reply without a tool call is read as the
/// JSON array of the diff-only output format. Findings without a valid `category` get the
/// specialist's own (none for the generalist).
struct SubmitFindings<'a> {
    def: ToolDef,
    allowed: HashMap<String, &'a CommentableRange>,
    anchor_retried: AtomicBool,
    /// The agent's specialist and batch index, for the category default and the logs.
    kind: SpecialistKind,
    batch: usize,
}

impl<'a> SubmitFindings<'a> {
    fn new(
        kind: SpecialistKind,
        batch: usize,
        allowed: HashMap<String, &'a CommentableRange>,
    ) -> Self {
        Self {
            def: submit_findings_tool_def(kind == SpecialistKind::Generalist),
            allowed,
            anchor_retried: AtomicBool::new(false),
            kind,
            batch,
        }
    }

    fn with_default_category(&self, mut suggestions: Vec<Suggestion>) -> Vec<Suggestion> {
        for s in &mut suggestions {
            s.category = s.category.or(self.kind.category());
        }
        suggestions
    }
}

impl SubmitSpec for SubmitFindings<'_> {
    type Output = FinderSubmission;

    fn tool_def(&self) -> &ToolDef {
        &self.def
    }

    fn parse_arguments(
        &self,
        arguments: &Value,
        can_retry: bool,
    ) -> Result<FinderSubmission, String> {
        let suggestions = parse_submit_findings(arguments)?;
        if can_retry && !self.anchor_retried.swap(true, Ordering::Relaxed) {
            validate_batch_anchors(&suggestions, &self.allowed)?;
        }
        // Every parsed submission is logged, including one the completion gate later refuses; a call
        // bounced by the anchor check above returned early and is re-sent.
        let specialist = self.kind.label();
        let review_log = parse_review_log(arguments);
        match &review_log {
            Some(review_log) => tracing::debug!(
                specialist,
                batch = self.batch,
                review_log = %review_log,
                "finder review log"
            ),
            None => tracing::debug!(specialist, batch = self.batch, "finder review log missing"),
        }
        let dismissed = parse_dismissed(arguments);
        for d in &dismissed {
            tracing::debug!(
                specialist,
                batch = self.batch,
                file = %d.file,
                line = ?d.line,
                suspicion = %d.suspicion,
                why_safe = %d.why_safe,
                "finder dismissed"
            );
        }
        tracing::debug!(
            specialist,
            batch = self.batch,
            dismissed = dismissed.len(),
            "finder dismissals"
        );
        Ok(FinderSubmission {
            findings: self.with_default_category(suggestions),
            prose: review_log,
        })
    }

    fn parse_prose(&self, text: &str) -> Result<FinderSubmission, String> {
        let suggestions = parse_suggestions(text).map_err(|err| err.to_string())?;
        let prose = Some(text.trim())
            .filter(|text| !text.is_empty())
            .map(|text| redact_secrets(text).into_owned());
        Ok(FinderSubmission {
            findings: self.with_default_category(suggestions),
            prose,
        })
    }

    fn has_findings(&self, output: &FinderSubmission) -> bool {
        !output.findings.is_empty()
    }

    fn with_findings_of(
        &self,
        accepted: FinderSubmission,
        earlier: FinderSubmission,
    ) -> FinderSubmission {
        FinderSubmission {
            findings: earlier.findings,
            ..accepted
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};
    use std::sync::Arc;
    use std::time::Duration;

    use adus_core::types::ReviewMode;
    use adus_git::{FileDiff, FileStatus, PullRequest};
    use adus_llm::router::UsageContext;
    use adus_llm::{Message, ReasoningEffort};
    use serde_json::json;
    use tokio::time::Instant;
    use uuid::Uuid;

    use super::*;
    use crate::agent::AgentLlm;
    use crate::parse::CommentableRange;
    use crate::test_support::{ScriptedChat, call, msg_text, text_turn, tools_turn, user_text};
    use crate::workspace::RepoWorkspace;

    fn sample_pr() -> PullRequest {
        PullRequest {
            number: 7,
            title: "Test".into(),
            description: "Desc".into(),
            head_sha: "h1".into(),
            base_sha: "b1".into(),
            is_draft: false,
            source_ref: None,
        }
    }

    fn finding_arg(file: &str, line: u32, title: &str) -> serde_json::Value {
        json!({
            "file": file,
            "line": line,
            "severity": "warning",
            "title": title,
            "body": "why",
        })
    }

    fn submit_turn(
        id: &str,
        findings: Vec<serde_json::Value>,
    ) -> Result<adus_llm::ChatResponse, adus_llm::router::ChatError> {
        tools_turn(vec![call(
            id,
            "submit_findings",
            json!({ "findings": findings }),
        )])
    }

    const A_PATCH: &str = "@@ -1,1 +1,3 @@\n a\n+b\n+c\n";

    /// `src/a.rs` with lines 2 and 3 added.
    fn a_rs() -> FileDiff {
        FileDiff {
            path: "src/a.rs".into(),
            old_path: None,
            status: FileStatus::Modified,
            patch: Some(A_PATCH.into()),
        }
    }

    fn a_rs_allowed() -> HashMap<String, CommentableRange> {
        HashMap::from([(
            "src/a.rs".to_string(),
            CommentableRange::from_patch(A_PATCH),
        )])
    }

    /// The changed `src/a.rs` and an unchanged `src/b.rs` next to it.
    fn repo_toolbox() -> Toolbox {
        Toolbox::new(Arc::new(RepoWorkspace::from_files(&[
            ("src/a.rs", b"a\nb\nc\n"),
            ("src/b.rs", b"fn caller() {}\n"),
        ])))
    }

    fn batch_allowed_of<'a>(
        files: &[&FileDiff],
        allowed: &'a HashMap<String, CommentableRange>,
    ) -> HashMap<String, &'a CommentableRange> {
        batch_allowed(
            &DiffBatch {
                files: files.to_vec(),
                rules: Vec::new(),
            },
            allowed,
        )
    }

    fn finder_llm<'a>(
        chat: &'a ScriptedChat,
        usage: &'a UsageContext,
        token_budget: Option<&'a Arc<TokenBudget>>,
    ) -> FinderLlm<'a> {
        FinderLlm {
            primary: LlmContext {
                chat,
                provider_id: Uuid::from_u128(1),
                model: "m",
                usage,
                token_budget,
            },
            fast_model: "fast",
            effort: ReasoningEffort::High,
        }
    }

    fn pass1_job<'a>(kind: SpecialistKind, files: &'a [&'a FileDiff]) -> FinderJob<'a> {
        FinderJob {
            kind,
            prompt: "PROMPT".into(),
            files,
            max_steps: finder_max_steps(files.len()),
            rescue: false,
        }
    }

    #[tokio::test]
    async fn submit_findings_accepts_invalid_anchors_on_last_step() {
        let allowed = a_rs_allowed();
        let usage = UsageContext {
            review_id: Some(Uuid::from_u128(1)),
            repository_id: Some(Uuid::from_u128(2)),
        };
        let toolbox = repo_toolbox();
        let mut batch_map = HashMap::new();
        batch_map.insert("src/a.rs".to_string(), &allowed["src/a.rs"]);
        let submit = SubmitFindings::new(SpecialistKind::Generalist, 0, batch_map);
        let chat = ScriptedChat::new(vec![submit_turn(
            "s1",
            vec![
                finding_arg("src/a.rs", 3, "Valid on last step"),
                finding_arg("src/a.rs", 1, "Invalid on last step"),
            ],
        )]);
        let agent_llm = AgentLlm {
            chat: &chat,
            provider_id: Uuid::from_u128(1),
            model: "m",
            system: "sys",
            effort: Some(ReasoningEffort::Medium),
            usage: &usage,
        };
        let outcome = run_react_loop(
            &agent_llm,
            AgentTools::Full(&toolbox),
            &submit,
            &AgentConfig::new(1),
            "PROMPT".into(),
        )
        .await
        .expect("final step must not fail Unparseable on invalid anchor");
        assert_eq!(outcome.steps_used, 1);
        assert_eq!(outcome.result.findings.len(), 2);
    }

    #[tokio::test]
    async fn finder_ledger_only_for_generalist_and_bug() {
        let file = a_rs();
        let files = vec![&file];
        let allowed = a_rs_allowed();
        let usage = UsageContext::default();
        let toolbox = repo_toolbox();
        let deadline = Instant::now() + Duration::from_secs(30);
        let is_debt =
            |m: &Message| msg_text(m).is_some_and(|t| t.starts_with("Unread changed code."));

        for (kind, gated) in [
            (SpecialistKind::Generalist, true),
            (SpecialistKind::Bug, true),
            (SpecialistKind::Security, false),
            (SpecialistKind::Performance, false),
        ] {
            let chat = ScriptedChat::new(vec![
                tools_turn(vec![call("g1", "grep", json!({ "pattern": "b" }))]),
                submit_turn("s1", vec![finding_arg("src/a.rs", 2, "Early")]),
                submit_turn("s2", vec![finding_arg("src/a.rs", 3, "Again")]),
            ]);
            let submit = SubmitFindings::new(kind, 0, batch_allowed_of(&files, &allowed));
            let outcome = run_one(
                finder_llm(&chat, &usage, None),
                pass1_job(kind, &files),
                AgentTools::Full(&toolbox),
                &submit,
                deadline,
            )
            .await
            .expect("finder run succeeds");
            let reqs = chat.requests();
            assert_eq!(outcome.gate_refused, gated, "{kind:?}");
            assert_eq!(outcome.steps_used, if gated { 3 } else { 2 }, "{kind:?}");
            assert_eq!(
                outcome.result.findings[0].title,
                if gated { "Again" } else { "Early" },
                "{kind:?}"
            );
            // No `category` was sent: a specialist's own fills in, the generalist's stays unset.
            assert_eq!(
                outcome.result.findings[0].category,
                kind.category(),
                "{kind:?}"
            );
            assert_eq!(reqs[1].messages.iter().any(is_debt), gated, "{kind:?}");
            assert!(
                reqs.iter()
                    .all(|r| r.messages.iter().filter(|m| is_debt(m)).count() <= 1)
            );
        }
    }

    #[tokio::test]
    async fn finder_runs_synthesis_rescue_and_merges_missed_findings() {
        let pr = sample_pr();
        let file = a_rs();
        let batches = vec![DiffBatch {
            files: vec![&file],
            rules: Vec::new(),
        }];
        let allowed = a_rs_allowed();
        let usage = UsageContext::default();
        let input = FinderInput {
            pr: &pr,
            manifest: "src/a.rs",
            batches: &batches,
            allowed: &allowed,
            memories: &[],
            repo_instructions: None,
            mode: ReviewMode::Generalist,
        };
        let toolbox = repo_toolbox();
        let secret = format!("ghp_{}", "A".repeat(36));
        let long_pattern = format!("{secret} {}", "x".repeat(200));
        let chat = ScriptedChat::new(vec![
            // Pass 1 reads the changed file and submits one finding (the gate accepts it).
            tools_turn(vec![
                call("r1", "read_file", json!({ "path": "src/a.rs" })),
                call("g1", "grep", json!({ "pattern": long_pattern })),
            ]),
            submit_turn("s1", vec![finding_arg("src/a.rs", 2, "First")]),
            // Pass 2 opens a caller, then submits a missed finding and a repeat of Pass 1's.
            tools_turn(vec![call("r2", "read_file", json!({ "path": "src/b.rs" }))]),
            submit_turn(
                "s2",
                vec![
                    finding_arg("src/a.rs", 3, "Missed"),
                    finding_arg("src/a.rs", 2, "First again"),
                ],
            ),
        ]);
        let deadline = Instant::now() + Duration::from_secs(600);
        let run = run_finder(
            finder_llm(&chat, &usage, None),
            &input,
            Some(&toolbox),
            deadline,
        )
        .await;

        let outcome = run.tasks[0].result.as_ref().expect("both passes succeed");
        let merged: Vec<_> = outcome
            .result
            .iter()
            .map(|f| (f.suggestion.line, f.origin))
            .collect();
        assert_eq!(
            merged,
            [(2, FinderPass::Pass1), (3, FinderPass::SynthesisRescue)]
        );
        assert_eq!(run.synthesis_candidates, 1);
        assert_eq!(run.synthesis_skipped, None);
        assert_eq!(outcome.steps_used, 4);
        assert_eq!(
            outcome.opened_files,
            HashSet::from(["src/a.rs".to_owned(), "src/b.rs".to_owned()])
        );

        let reqs = chat.requests();
        assert_eq!(reqs.len(), 4);
        // Pass 2 resends Pass 1's prompt unchanged, then the three blocks.
        let pass2 = user_text(&reqs[2]);
        let prefix = format!(
            "{}\n\n<already_inspected_files>\nsrc/a.rs\n</already_inspected_files>\n<recent_investigation>\nread_file({{\"path\":\"src/a.rs\"}})\ngrep(",
            user_text(&reqs[0])
        );
        assert!(pass2.starts_with(&prefix), "{pass2}");
        assert!(pass2.contains("\n<current_findings>\n- src/a.rs:2: why\n</current_findings>"));
        let grep_line = pass2
            .lines()
            .find(|line| line.starts_with("grep("))
            .expect("grep call summarized");
        assert!(!grep_line.contains(&secret), "{grep_line}");
        let args = &grep_line["grep(".len()..grep_line.len() - 1];
        assert_eq!(args.chars().count(), 150);
        assert!(args.ends_with("..."));
        // Pass 2 is not gated: its submit is accepted although Pass 2 never read `src/a.rs`
        // (a refusal would need a fifth step), and it gets no debt notes.
        assert!(reqs[2..].iter().all(|r| {
            r.messages
                .iter()
                .all(|m| msg_text(m).is_none_or(|t| !t.starts_with("Unread changed code.")))
        }));
    }

    #[tokio::test]
    async fn finder_skips_synthesis_rescue_on_budget_time_or_specialist() {
        let file = a_rs();
        let files = vec![&file];
        let allowed = a_rs_allowed();
        let usage = UsageContext::default();
        let toolbox = repo_toolbox();
        // Pass 1 adds 200 input tokens (two steps of 100) to `used_before`, out of 10_000.
        for (kind, used_before, secs_left, expected) in [
            (SpecialistKind::Security, None, 600, None),
            (SpecialistKind::Performance, None, 600, None),
            (
                SpecialistKind::Generalist,
                Some(7_800),
                600,
                Some(SynthesisSkipped::Budget),
            ),
            // A near-zero mean step: the 60 s floor decides.
            (
                SpecialistKind::Generalist,
                None,
                50,
                Some(SynthesisSkipped::Time),
            ),
            (SpecialistKind::Bug, Some(6_800), 600, None),
        ] {
            let rescues = matches!(kind, SpecialistKind::Generalist | SpecialistKind::Bug)
                && expected.is_none();
            let chat = ScriptedChat::new(vec![
                tools_turn(vec![call("r1", "read_file", json!({ "path": "src/a.rs" }))]),
                submit_turn("s1", vec![finding_arg("src/a.rs", 2, "First")]),
                submit_turn("s2", Vec::new()),
            ]);
            let budget = used_before.map(|used| {
                let budget = Arc::new(TokenBudget::new(10_000));
                budget.add(used, 0, 0);
                budget
            });
            let deadline = Instant::now() + Duration::from_secs(secs_left);
            let task = run_task(
                finder_llm(&chat, &usage, budget.as_ref()),
                pass1_job(kind, &files),
                AgentTools::Full(&toolbox),
                batch_allowed_of(&files, &allowed),
                0,
                deadline,
            )
            .await;
            assert_eq!(task.synthesis_skipped, expected, "{kind:?}");
            assert_eq!(task.synthesis_candidates, 0, "{kind:?}");
            assert_eq!(
                chat.requests().len(),
                if rescues { 3 } else { 2 },
                "{kind:?}"
            );
            assert_eq!(task.result.expect("pass 1 succeeds").result.len(), 1);
        }

        // The time window is six mean Pass 1 steps (90 s each when none was measured), at
        // least 60 s; the budget share is inclusive at 25 %.
        let secs = Duration::from_secs;
        for (time_left, mean_step, expected) in [
            (59, Some(secs(2)), Err(SynthesisSkipped::Time)),
            (60, Some(secs(2)), Ok(())),
            (119, Some(secs(20)), Err(SynthesisSkipped::Time)),
            (120, Some(secs(20)), Ok(())),
            (539, None, Err(SynthesisSkipped::Time)),
            (540, None, Ok(())),
        ] {
            assert_eq!(
                synthesis_gate(None, secs(time_left), mean_step),
                expected,
                "{time_left}s left, mean step {mean_step:?}"
            );
        }
        let budget = TokenBudget::new(100);
        budget.add(75, 0, 0);
        assert_eq!(synthesis_gate(Some(&budget), secs(600), None), Ok(()));
        budget.add(1, 0, 0);
        assert_eq!(
            synthesis_gate(Some(&budget), secs(600), None),
            Err(SynthesisSkipped::Budget)
        );
    }

    #[tokio::test]
    async fn prose_recovery_extracts_findings_when_pass_omits_structured_array() {
        let file = a_rs();
        let files = vec![&file];
        let allowed = a_rs_allowed();
        let usage = UsageContext::default();
        let toolbox = repo_toolbox();
        let review_log = "Bug at src/a.rs:2: the new value b is never validated, so the caller crashes on empty input. It should be checked before use.";
        assert!(looks_like_findings(review_log));
        // Too short, and long enough with a single signal group (file references).
        assert!(!looks_like_findings("Bug at src/a.rs:2, should fix."));
        assert!(!looks_like_findings(
            "I read src/a.rs:1 and src/b.rs:1 and followed every caller of the changed function through the module."
        ));
        let chat = ScriptedChat::new(vec![
            tools_turn(vec![call("r1", "read_file", json!({ "path": "src/a.rs" }))]),
            tools_turn(vec![call(
                "s1",
                "submit_findings",
                json!({ "review_log": review_log, "findings": [] }),
            )]),
            text_turn(
                r#"[{"file": "src/a.rs", "line": 2, "body": "b is never validated"},
                    {"file": "src/other.rs", "line": 1, "body": "outside the batch"}]"#,
            ),
        ]);
        let deadline = Instant::now() + Duration::from_secs(600);
        let task = run_task(
            finder_llm(&chat, &usage, None),
            pass1_job(SpecialistKind::Security, &files),
            AgentTools::Full(&toolbox),
            batch_allowed_of(&files, &allowed),
            0,
            deadline,
        )
        .await;
        let found = task.result.expect("pass 1 succeeds").result;
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].origin, FinderPass::ProseRecovery);
        assert_eq!(found[0].suggestion.file, "src/a.rs");
        assert_eq!(found[0].suggestion.line, 2);
        assert_eq!(
            found[0].suggestion.category,
            SpecialistKind::Security.category()
        );

        let reqs = chat.requests();
        assert_eq!(reqs.len(), 3);
        let recovery = &reqs[2];
        assert_eq!(recovery.model, "fast");
        assert_eq!(recovery.effort, Some(ReasoningEffort::Medium));
        assert_eq!(recovery.max_tokens, 2_048);
        assert!(recovery.tools.is_empty());
        assert!(user_text(recovery).contains(review_log));
    }

    #[test]
    fn finder_max_steps_scales_with_batch_files() {
        for (files, steps) in [
            (0, 20),
            (8, 20),
            (9, 21),
            (10, 21),
            (11, 22),
            (28, 30),
            (100, 30),
        ] {
            assert_eq!(finder_max_steps(files), steps, "{files} files");
        }
    }
}
