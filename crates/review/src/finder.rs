//! First pass of the review: Finder agents look for defects in each diff batch. Batches run
//! [`BATCH_CONCURRENCY`] at a time and the specialists of one batch run in parallel. Every
//! agent must finish by the Finder deadline: it wraps up (submits) when the deadline gets
//! close, and one still running at the deadline counts as a failed task.
//! When the model rejects tool calling, the whole Finder is run again without tools.
//! A batch that would start once the review's token budget is exhausted is not started.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use adus_core::types::{ReviewMode, normalize_path};
use adus_db::memories::Memory;
use adus_git::PullRequest;
use adus_llm::ReasoningEffort;
use adus_llm::ToolDef;
use adus_llm::router::{ChatService, UsageContext};
use futures::{StreamExt, future, stream};
use serde_json::Value;
use tokio::time::Instant;
use tracing::Instrument;
use uuid::Uuid;

use crate::agent::run_react_loop;
use crate::agent::{AgentConfig, AgentError, AgentLlm, AgentOutcome, AgentTools, SubmitSpec};
use crate::budget::TokenBudget;
use crate::parse::{
    CommentableRange, Suggestion, parse_dismissed, parse_submit_findings, parse_suggestions,
    submit_findings_tool_def, validate_batch_anchors,
};
use crate::prompt::{DiffBatch, build_finder_system_prompt, build_finder_user_prompt};
use crate::specialists::{SpecialistKind, active_specialists};
use crate::tools::Toolbox;

/// Diff batches reviewed at the same time (spec 4.7).
const BATCH_CONCURRENCY: usize = 2;
/// LLM calls a Finder agent may make on a batch of up to `FINDER_FREE_FILES` files.
const FINDER_BASE_STEPS: usize = 20;
/// Files per batch covered by `FINDER_BASE_STEPS`; each two files beyond add one step.
const FINDER_FREE_FILES: usize = 8;
/// Most LLM calls one Finder agent may make, however large the batch.
const FINDER_STEP_CAP: usize = 30;
/// The last Finder steps that offer only `submit_findings`.
const FINDER_WRAP_UP_STEPS: usize = 2;

/// LLM calls one Finder agent may make on a batch of `files` files.
pub(crate) fn finder_max_steps(files: usize) -> usize {
    (FINDER_BASE_STEPS + files.saturating_sub(FINDER_FREE_FILES).div_ceil(2)).min(FINDER_STEP_CAP)
}

/// Where the Finder talks to the model: the provider's `primary_model` at `finder_effort(mode)`.
#[derive(Clone, Copy)]
pub(crate) struct FinderLlm<'a> {
    pub chat: &'a dyn ChatService,
    pub provider_id: Uuid,
    pub model: &'a str,
    pub effort: ReasoningEffort,
    pub usage: &'a UsageContext,
    /// The review's shared input-token budget, if any.
    pub token_budget: Option<&'a Arc<TokenBudget>>,
}

/// What every Finder agent of one review is shown.
pub(crate) struct FinderInput<'a> {
    pub pr: &'a PullRequest,
    pub manifest: &'a str,
    pub batches: &'a [DiffBatch<'a>],
    pub allowed: &'a HashMap<String, CommentableRange>,
    pub expanded: &'a HashMap<String, String>,
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

/// Why a Finder task has no result.
#[derive(Debug)]
pub(crate) enum FinderFailure {
    /// The Finder deadline passed first.
    TimedOut,
    /// The batch was not started: the review's token budget was already exhausted.
    TokenBudget,
    Agent(AgentError),
}

/// One Finder agent: a specialist on one batch.
#[derive(Debug)]
pub(crate) struct FinderTask {
    /// Index into `FinderInput::batches`.
    pub batch: usize,
    pub kind: SpecialistKind,
    pub result: Result<AgentOutcome<Vec<Suggestion>>, FinderFailure>,
}

#[derive(Debug)]
pub(crate) struct FinderRun {
    /// Ordered by (batch index, specialist order).
    pub tasks: Vec<FinderTask>,
    /// The model rejected tool calling, so `tasks` come from the re-run without tools.
    pub tools_unsupported: bool,
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
    let submit_def = submit_findings_tool_def();
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
                input.expanded,
                with_tools,
            );
            let kinds = active_specialists(
                input.mode,
                !batch.rules.is_empty() || input.repo_instructions.is_some(),
            );
            let allowed = batch_allowed(batch, input.allowed);
            let submit_def = &submit_def;
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
                    .map(|kind| {
                        let submit = SubmitFindings {
                            def: submit_def.clone(),
                            allowed: allowed.clone(),
                            anchor_retried: AtomicBool::new(false),
                            specialist: kind.label(),
                            batch: index,
                        };
                        let max_steps = finder_max_steps(batch.files.len());
                        run_one(
                            llm,
                            kind,
                            prompt.clone(),
                            tools,
                            submit,
                            max_steps,
                            deadline,
                        )
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

/// One specialist on one batch, cut at `deadline`.
async fn run_one(
    llm: FinderLlm<'_>,
    kind: SpecialistKind,
    prompt: String,
    tools: AgentTools<'_>,
    submit: SubmitFindings<'_>,
    max_steps: usize,
    deadline: Instant,
) -> FinderTask {
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
        ..AgentConfig::new(max_steps)
    };
    // Names the agent on every log line of its run (concurrent agents interleave).
    let span = tracing::debug_span!("finder", specialist = kind.label(), batch = submit.batch);
    let run = run_react_loop(&agent, tools, &submit, &config, prompt).instrument(span);
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
    FinderTask {
        batch: submit.batch,
        kind,
        result,
    }
}

/// The Finder's terminal tool, `submit_findings`. A reply without a tool call is read as the
/// JSON array of the diff-only output format.
struct SubmitFindings<'a> {
    def: ToolDef,
    allowed: HashMap<String, &'a CommentableRange>,
    anchor_retried: AtomicBool,
    /// The agent's `SpecialistKind::label` and batch index, for the dismissal logs.
    specialist: &'static str,
    batch: usize,
}

impl SubmitSpec for SubmitFindings<'_> {
    type Output = Vec<Suggestion>;

    fn tool_def(&self) -> &ToolDef {
        &self.def
    }

    fn parse_arguments(
        &self,
        arguments: &Value,
        can_retry: bool,
    ) -> Result<Vec<Suggestion>, String> {
        let suggestions = parse_submit_findings(arguments)?;
        if can_retry && !self.anchor_retried.swap(true, Ordering::Relaxed) {
            validate_batch_anchors(&suggestions, &self.allowed)?;
        }
        // Only the accepted call is logged: a bounced one returned above and is re-sent.
        let dismissed = parse_dismissed(arguments);
        for d in &dismissed {
            tracing::debug!(
                specialist = self.specialist,
                batch = self.batch,
                file = %d.file,
                line = ?d.line,
                suspicion = %d.suspicion,
                why_safe = %d.why_safe,
                "finder dismissed"
            );
        }
        tracing::debug!(
            specialist = self.specialist,
            batch = self.batch,
            dismissed = dismissed.len(),
            "finder dismissals"
        );
        Ok(suggestions)
    }

    fn parse_prose(&self, text: &str) -> Result<Vec<Suggestion>, String> {
        parse_suggestions(text).map_err(|err| err.to_string())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::Duration;

    use adus_core::types::{ReviewMode, Severity};
    use adus_git::{FileDiff, FileStatus, PullRequest};
    use adus_llm::{Message, ReasoningEffort};
    use serde_json::json;
    use tokio::time::Instant;
    use uuid::Uuid;

    use super::*;
    use crate::parse::CommentableRange;
    use crate::test_support::{ScriptedChat, call, tools_turn};
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

    #[tokio::test]
    async fn finder_bounces_off_line_anchor_once_then_accepts_partial_on_second_or_last_step() {
        let pr = sample_pr();
        let patch = "@@ -1,1 +1,3 @@\n a\n+b\n+c\n";
        let file = FileDiff {
            path: "src/a.rs".into(),
            old_path: None,
            status: FileStatus::Modified,
            patch: Some(patch.into()),
        };
        let batches = vec![DiffBatch {
            files: vec![&file],
            rules: Vec::new(),
        }];
        let allowed =
            HashMap::from([("src/a.rs".to_string(), CommentableRange::from_patch(patch))]);
        let usage = UsageContext {
            review_id: Some(Uuid::from_u128(1)),
            repository_id: Some(Uuid::from_u128(2)),
        };
        let expanded = HashMap::new();
        let input = FinderInput {
            pr: &pr,
            manifest: "src/a.rs",
            batches: &batches,
            allowed: &allowed,
            expanded: &expanded,
            memories: &[],
            repo_instructions: None,
            mode: ReviewMode::Generalist,
        };
        let toolbox = Toolbox::new(Arc::new(RepoWorkspace::from_files(&[(
            "src/a.rs",
            b"a\nb\nc\n",
        )])));
        let deadline = Instant::now() + Duration::from_secs(30);

        // 1. First submit on off-line anchor bounces with nearest '+' lines; second submit on valid line succeeds.
        let chat = ScriptedChat::new(vec![
            submit_turn("s1", vec![finding_arg("src/a.rs", 1, "Off-line")]),
            submit_turn("s2", vec![finding_arg("src/a.rs", 2, "Re-anchored")]),
        ]);
        let llm = FinderLlm {
            chat: &chat,
            provider_id: Uuid::from_u128(1),
            model: "m",
            effort: ReasoningEffort::Medium,
            usage: &usage,
            token_budget: None,
        };
        let run = run_finder(llm, &input, Some(&toolbox), deadline).await;
        assert_eq!(run.tasks.len(), 1);
        let outcome = run.tasks[0]
            .result
            .as_ref()
            .expect("re-anchored submit succeeds");
        assert_eq!(outcome.steps_used, 2);
        assert_eq!(outcome.result.len(), 1);
        assert_eq!(outcome.result[0].line, 2);
        assert_eq!(outcome.result[0].title, "Re-anchored");
        let reqs = chat.requests();
        assert_eq!(reqs.len(), 2);
        let tool_err = reqs[1]
            .messages
            .iter()
            .find_map(|m| match m {
                Message::Tool { results } => {
                    results.first().map(|r| (r.is_error, r.content.as_str()))
                }
                _ => None,
            })
            .expect("bounce tool error");
        assert!(tool_err.0);
        assert!(tool_err.1.contains("2, 3"), "{}", tool_err.1);

        // 2. Second submit with mixed valid + invalid anchors succeeds without AgentError::Unparseable.
        let chat = ScriptedChat::new(vec![
            submit_turn("s1", vec![finding_arg("src/a.rs", 1, "Off-line")]),
            submit_turn(
                "s2",
                vec![
                    finding_arg("src/a.rs", 2, "Valid"),
                    finding_arg("src/a.rs", 1, "Still off-line"),
                ],
            ),
        ]);
        let llm = FinderLlm {
            chat: &chat,
            provider_id: Uuid::from_u128(1),
            model: "m",
            effort: ReasoningEffort::Medium,
            usage: &usage,
            token_budget: None,
        };
        let run = run_finder(llm, &input, Some(&toolbox), deadline).await;
        let outcome = run.tasks[0]
            .result
            .as_ref()
            .expect("second submit with partial invalid keeps findings");
        assert_eq!(outcome.steps_used, 2);
        assert_eq!(outcome.result.len(), 2);
        assert_eq!(outcome.result[0].severity, Severity::Warning);

        // 3. On the final step (can_retry == false), SubmitFindings accepts mixed findings immediately.
        let mut batch_map = HashMap::new();
        batch_map.insert("src/a.rs".to_string(), &allowed["src/a.rs"]);
        let submit = SubmitFindings {
            def: submit_findings_tool_def(),
            allowed: batch_map,
            anchor_retried: AtomicBool::new(false),
            specialist: "Generalist",
            batch: 0,
        };
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
        assert_eq!(outcome.result.len(), 2);
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
