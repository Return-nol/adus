//! Evidence loop for one sharded team-rule violation. Step A, the collector, is a short agent
//! run that reads the repository and records every numbered `read_file` excerpt it saw. Step B,
//! the judge, is a separate single-turn call that rules on the violation from those excerpts and
//! the related diffs. A violation survives only when the judge says `violated` and cites a line
//! that literally appears in an excerpt (the citation gate). Every failure drops the violation:
//! no rule finding is posted without evidence.

use std::sync::Arc;

use adus_core::types::normalize_path;
use adus_db::rules::Rule;
use adus_git::FileDiff;
use adus_llm::router::{ChatService, UsageContext};
use adus_llm::{ReasoningEffort, ToolDef};
use serde_json::{Value, json};
use tokio::time::Instant;
use tracing::Instrument;
use uuid::Uuid;

use crate::agent::{
    AgentConfig, AgentError, AgentLlm, AgentTools, SubmitSpec, chat_single_turn, run_react_loop,
};
use crate::budget::TokenBudget;
use crate::parse::parse_u32_field;
use crate::prompt::{diff_block, severity_name, truncate_with_marker, wrap};
use crate::redact::redact_secrets;
use crate::rules_judge::RawShardViolation;
use crate::tools::Toolbox;

/// LLM calls of one collector run.
const RULE_EVIDENCE_MAX_STEPS: usize = 6;
/// Characters of deduplicated excerpts shown to the judge.
const MAX_RULE_EVIDENCE_CHARS: usize = 24_000;
/// Characters of related diffs shown to the judge.
const RULE_RELATED_PATCH_CHARS: usize = 8_000;
/// Output tokens of the judge call (the client adds the `High` reasoning reserve).
const RULE_JUDGE_MAX_TOKENS: u32 = 1_024;
/// Effort of the collector and the judge (spec §2: fast @ High).
const RULE_EVIDENCE_EFFORT: ReasoningEffort = ReasoningEffort::High;
/// Trace label of the judge call.
const RULE_JUDGE_CALL: &str = "rule_judge";
/// The collector's terminal tool.
const SUBMIT_EVIDENCE: &str = "submit_evidence";
/// Characters of a rationale kept in a debug log.
const RATIONALE_LOG_CHARS: usize = 300;

/// One successful `read_file` output of the collector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReadExcerpt {
    /// Normalized repository path.
    pub path: String,
    /// The tool output: a `[path: lines X-Y of Z]` header, then `{n}: {line}` rows.
    pub content: String,
}

/// What the evidence loop decided about one candidate violation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum EvidenceVerdict {
    /// The judge found the rule violated and cited a line the collector read.
    Verified {
        path: String,
        line: u32,
        /// The judge's rationale, secret-redacted.
        rationale: String,
    },
    /// The collector refuted the violation with high confidence, citing a line it read.
    Refuted,
    /// The judge said `violated` but cited no line the collector read.
    UnsupportedCitation,
    /// Dropped for another reason: the judge's verdict, or why the loop could not finish.
    Rejected(&'static str),
}

/// Where the evidence loop talks to the model and reads the repository.
#[derive(Clone, Copy)]
pub(crate) struct RuleEvidenceContext<'a> {
    pub chat: &'a dyn ChatService,
    pub provider_id: Uuid,
    pub fast_model: &'a str,
    /// Cloned into every `chat` call.
    pub usage: &'a UsageContext,
    /// The review's shared input-token budget, if any.
    pub token_budget: Option<&'a Arc<TokenBudget>>,
    pub toolbox: &'a Toolbox,
}

/// Whether some excerpt of `path` shows `line` as a `{line}: ` row with non-blank text.
pub(crate) fn read_contains_line(excerpts: &[ReadExcerpt], path: &str, line: u32) -> bool {
    let path = normalized(path);
    let prefix = format!("{line}: ");
    excerpts
        .iter()
        .filter(|excerpt| normalized(&excerpt.path) == path)
        .any(|excerpt| {
            excerpt.content.lines().any(|row| {
                row.strip_prefix(&prefix)
                    .is_some_and(|text| !text.trim().is_empty())
            })
        })
}

/// The collector refuted the violation: `keep` is false at `high` confidence and the
/// rationale cites at least one `path:line` that [`read_contains_line`] finds in `excerpts`.
pub(crate) fn has_source_refutation(
    keep: bool,
    confidence: Option<&str>,
    rationale: &str,
    excerpts: &[ReadExcerpt],
) -> bool {
    !keep
        && confidence == Some("high")
        && cited_locations(rationale).any(|(path, line)| read_contains_line(excerpts, path, line))
}

/// Runs the collector, then (unless it refuted the violation or failed) the judge, both
/// bounded by `deadline`. `file` is the file `candidate` is on; `all_files` are the review's
/// changed files, whose diffs the judge sees after `file`'s.
pub(crate) async fn verify_rule_candidate(
    ctx: RuleEvidenceContext<'_>,
    rule: &Rule,
    file: &FileDiff,
    candidate: &RawShardViolation,
    all_files: &[FileDiff],
    deadline: Instant,
) -> EvidenceVerdict {
    let span = tracing::debug_span!(
        "rule_evidence",
        rule = %rule.title,
        file = %file.path,
        line = candidate.line
    );
    let verdict = async {
        let collected = tokio::time::timeout_at(
            deadline,
            collect_evidence(ctx, rule, file, candidate, deadline),
        )
        .await;
        let (submission, excerpts) = match collected {
            Ok(Ok(collected)) => collected,
            Ok(Err(reason)) => return EvidenceVerdict::Rejected(reason),
            Err(_) => return EvidenceVerdict::Rejected("collector_timeout"),
        };
        tracing::debug!(
            keep = submission.keep,
            confidence = submission.confidence,
            rationale = %log_text(&submission.rationale),
            excerpts = excerpts.len(),
            "rule evidence collected"
        );
        if has_source_refutation(
            submission.keep,
            submission.confidence,
            &submission.rationale,
            &excerpts,
        ) {
            return EvidenceVerdict::Refuted;
        }
        let excerpts = capped_excerpts(excerpts);
        if excerpts.is_empty() {
            return EvidenceVerdict::Rejected("no_excerpts");
        }
        let prompt = judge_prompt(rule, file, candidate, &excerpts, all_files);
        match tokio::time::timeout_at(deadline, judge(ctx, prompt)).await {
            Ok(Ok(reply)) => judge_verdict(&reply, &excerpts),
            Ok(Err(reason)) => EvidenceVerdict::Rejected(reason),
            Err(_) => EvidenceVerdict::Rejected("judge_timeout"),
        }
    }
    .instrument(span.clone())
    .await;
    span.in_scope(|| tracing::debug!(verdict = ?verdict, "rule evidence verdict"));
    verdict
}

/// Step A: the collector run and the numbered excerpts it read, in read order.
async fn collect_evidence(
    ctx: RuleEvidenceContext<'_>,
    rule: &Rule,
    file: &FileDiff,
    candidate: &RawShardViolation,
    deadline: Instant,
) -> Result<(EvidenceSubmission, Vec<ReadExcerpt>), &'static str> {
    let llm = AgentLlm {
        chat: ctx.chat,
        provider_id: ctx.provider_id,
        model: ctx.fast_model,
        system: COLLECTOR_SYSTEM,
        effort: Some(RULE_EVIDENCE_EFFORT),
        usage: ctx.usage,
    };
    let config = AgentConfig {
        deadline: Some(deadline),
        token_budget: ctx.token_budget.cloned(),
        no_tools_on_last_step: true,
        capture_reads: true,
        ..AgentConfig::new(RULE_EVIDENCE_MAX_STEPS)
    };
    let submit = SubmitEvidence(submit_evidence_tool_def());
    let prompt = collector_prompt(rule, file, candidate);
    match run_react_loop(
        &llm,
        AgentTools::Full(ctx.toolbox),
        &submit,
        &config,
        prompt,
    )
    .await
    {
        Ok(outcome) => {
            let excerpts = outcome
                .read_outputs
                .into_iter()
                .filter(|(_, content)| content.lines().any(is_numbered_row))
                .map(|(path, content)| ReadExcerpt { path, content })
                .collect();
            Ok((outcome.result, excerpts))
        }
        Err(AgentError::Unparseable(_)) => Err("collector_unparseable"),
        Err(err) => {
            tracing::debug!(error = %err, "rule evidence collector failed");
            Err("collector_failed")
        }
    }
}

/// Step B: the judge's raw reply.
async fn judge(ctx: RuleEvidenceContext<'_>, prompt: String) -> Result<String, &'static str> {
    let llm = AgentLlm {
        chat: ctx.chat,
        provider_id: ctx.provider_id,
        model: ctx.fast_model,
        system: JUDGE_SYSTEM,
        effort: Some(RULE_EVIDENCE_EFFORT),
        usage: ctx.usage,
    };
    chat_single_turn(
        &llm,
        RULE_JUDGE_CALL,
        ctx.token_budget.map(Arc::as_ref),
        prompt,
        RULE_JUDGE_MAX_TOKENS,
    )
    .await
    .map_err(|err| {
        tracing::debug!(error = %err, "rule evidence judge failed");
        "judge_failed"
    })
}

/// The citation gate over the judge's reply: `Verified` only for `violated` with a cited line
/// that appears in `excerpts`.
fn judge_verdict(reply: &str, excerpts: &[ReadExcerpt]) -> EvidenceVerdict {
    let Some(value) = first_object(reply, |value| {
        value.get("verdict").is_some().then(|| value.clone())
    }) else {
        return EvidenceVerdict::Rejected("unparseable");
    };
    let verdict = match value
        .get("verdict")
        .and_then(Value::as_str)
        .map(|text| text.trim().to_ascii_lowercase())
        .as_deref()
    {
        Some("violated") => "violated",
        Some("satisfied") => "satisfied",
        Some("not_applicable") => "not_applicable",
        Some("insufficient_evidence") => "insufficient_evidence",
        _ => "unparseable",
    };
    if verdict != "violated" {
        return EvidenceVerdict::Rejected(verdict);
    }
    let path = value.get("path").and_then(Value::as_str);
    let line = value.get("line").and_then(parse_u32_field);
    let rationale = value
        .get("rationale")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    tracing::debug!(rationale = %log_text(rationale), "rule evidence judge verdict");
    match (path, line) {
        (Some(path), Some(line)) if read_contains_line(excerpts, path, line) => {
            EvidenceVerdict::Verified {
                path: normalized(path),
                line,
                rationale: redact_secrets(rationale).into_owned(),
            }
        }
        _ => EvidenceVerdict::UnsupportedCitation,
    }
}

/// `excerpts` without exact duplicates, in order, until [`MAX_RULE_EVIDENCE_CHARS`]; the
/// excerpt that crosses the cap keeps only its whole rows that fit.
fn capped_excerpts(excerpts: Vec<ReadExcerpt>) -> Vec<ReadExcerpt> {
    let mut kept: Vec<ReadExcerpt> = Vec::new();
    let mut budget = MAX_RULE_EVIDENCE_CHARS;
    for excerpt in excerpts {
        if kept.contains(&excerpt) {
            continue;
        }
        let chars = excerpt.content.chars().count();
        if chars <= budget {
            budget -= chars;
            kept.push(excerpt);
            continue;
        }
        let mut content = String::new();
        for row in excerpt.content.lines() {
            let needed = row.chars().count() + 1;
            if needed > budget {
                break;
            }
            budget -= needed;
            content.push_str(row);
            content.push('\n');
        }
        if content.lines().any(is_numbered_row) {
            kept.push(ReadExcerpt {
                path: excerpt.path,
                content,
            });
        }
        break;
    }
    kept
}

/// A `{n}: ...` row of a `read_file` output.
fn is_numbered_row(row: &str) -> bool {
    row.split_once(": ")
        .is_some_and(|(n, _)| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// Every `path:line` in `text`: a run of path characters, a colon, then digits.
fn cited_locations(text: &str) -> impl Iterator<Item = (&str, u32)> {
    text.match_indices(':').filter_map(move |(at, _)| {
        let rest = &text[at + 1..];
        let digits = &rest[..rest
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(rest.len())];
        let line = digits.parse().ok()?;
        let before = &text[..at];
        let start = before
            .char_indices()
            .rev()
            .find(|&(_, c)| !is_path_char(c))
            .map_or(0, |(i, c)| i + c.len_utf8());
        let path = &before[start..];
        (!path.is_empty()).then_some((path, line))
    })
}

fn is_path_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | '/')
}

/// `path` as the crate compares repository paths: trimmed, no leading `./` or `/`.
fn normalized(path: &str) -> String {
    normalize_path(path.trim().trim_start_matches("./"))
}

/// Secret-redacted text cut for a debug log.
fn log_text(text: &str) -> String {
    truncate_with_marker(&redact_secrets(text), RATIONALE_LOG_CHARS, "…").into_owned()
}

/// The first JSON object in `text` that `read` accepts.
fn first_object<T>(text: &str, read: impl Fn(&Value) -> Option<T>) -> Option<T> {
    text.match_indices('{').find_map(|(start, _)| {
        let value = serde_json::Deserializer::from_str(&text[start..])
            .into_iter::<Value>()
            .next()?
            .ok()?;
        read(&value)
    })
}

/// `<rule>` and `<candidate>` sections shared by the collector and the judge prompts.
fn rule_and_candidate(rule: &Rule, file: &FileDiff, candidate: &RawShardViolation) -> String {
    let mut lines = vec![format!("file: {}", file.path)];
    match candidate.start_line {
        Some(start) => lines.push(format!("lines: {start}-{}", candidate.line)),
        None => lines.push(format!("line: {}", candidate.line)),
    }
    if let Some(cited) = candidate.cited_line {
        lines.push(format!("originally reported at line: {cited}"));
    }
    lines.push(format!("claim: {}", candidate.claim));
    format!(
        "{}\n\n{}",
        wrap(
            "rule",
            &format!(
                "({}) {}\n{}",
                severity_name(rule.severity),
                rule.title,
                rule.body
            )
        ),
        wrap("candidate", &lines.join("\n"))
    )
}

fn collector_prompt(rule: &Rule, file: &FileDiff, candidate: &RawShardViolation) -> String {
    format!(
        "{}\n\n{}\n\nRead the code that decides whether the claim in <candidate> is true, then call {SUBMIT_EVIDENCE}.",
        rule_and_candidate(rule, file, candidate),
        wrap("diff", &diff_block(file))
    )
}

/// The candidate file's diff first, then the other changed files', cut to
/// [`RULE_RELATED_PATCH_CHARS`].
fn related_patches(file: &FileDiff, all_files: &[FileDiff]) -> String {
    let blocks: Vec<String> = std::iter::once(file)
        .chain(all_files.iter().filter(|other| other.path != file.path))
        .map(diff_block)
        .collect();
    truncate_with_marker(
        &blocks.join("\n\n"),
        RULE_RELATED_PATCH_CHARS,
        "\n[diffs truncated]",
    )
    .into_owned()
}

fn judge_prompt(
    rule: &Rule,
    file: &FileDiff,
    candidate: &RawShardViolation,
    excerpts: &[ReadExcerpt],
    all_files: &[FileDiff],
) -> String {
    let excerpts: Vec<&str> = excerpts
        .iter()
        .map(|excerpt| excerpt.content.as_str())
        .collect();
    format!(
        "{}\n\n{}\n\n{}",
        rule_and_candidate(rule, file, candidate),
        wrap("excerpts", &excerpts.join("\n\n")),
        wrap("diffs", &related_patches(file, all_files))
    )
}

/// The collector's submission.
#[derive(Debug, Clone, PartialEq, Eq)]
struct EvidenceSubmission {
    keep: bool,
    confidence: Option<&'static str>,
    rationale: String,
}

/// Reads a submission loosely: `keep` may be a boolean or `"true"` / `"false"`; an unknown
/// `confidence` counts as none.
fn loose_submission(value: &Value) -> Option<EvidenceSubmission> {
    let keep = match value.get("keep")? {
        Value::Bool(flag) => *flag,
        Value::String(text) => match text.trim().to_ascii_lowercase().as_str() {
            "true" => true,
            "false" => false,
            _ => return None,
        },
        _ => return None,
    };
    let confidence = match value
        .get("confidence")
        .and_then(Value::as_str)
        .map(|text| text.trim().to_ascii_lowercase())
        .as_deref()
    {
        Some("high") => Some("high"),
        Some("medium") => Some("medium"),
        Some("low") => Some("low"),
        _ => None,
    };
    let rationale = value
        .get("rationale")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_owned();
    Some(EvidenceSubmission {
        keep,
        confidence,
        rationale,
    })
}

/// The collector's terminal tool.
struct SubmitEvidence(ToolDef);

impl SubmitSpec for SubmitEvidence {
    type Output = EvidenceSubmission;

    fn tool_def(&self) -> &ToolDef {
        &self.0
    }

    fn parse_arguments(
        &self,
        arguments: &Value,
        _can_retry: bool,
    ) -> Result<EvidenceSubmission, String> {
        loose_submission(arguments).ok_or_else(|| {
            "missing or invalid \"keep\"; expected {\"keep\": true or false, \"confidence\": \"high\" | \"medium\" | \"low\", \"rationale\": \"...\"}".to_string()
        })
    }

    fn parse_prose(&self, text: &str) -> Result<EvidenceSubmission, String> {
        first_object(text, loose_submission).ok_or_else(|| "no \"keep\" found".to_string())
    }
}

/// Uses only `type`, `properties`, `required`, `enum` and `description`.
fn submit_evidence_tool_def() -> ToolDef {
    ToolDef {
        name: SUBMIT_EVIDENCE.into(),
        description: "Submit what the code you read shows about the reported violation and end \
            the run. Call it exactly once."
            .into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "keep": {
                    "type": "boolean",
                    "description": "false only when code you read shows the rule is not broken; otherwise true."
                },
                "confidence": {
                    "type": "string",
                    "enum": ["high", "medium", "low"],
                    "description": "How sure the code you read makes you."
                },
                "rationale": {
                    "type": "string",
                    "description": "One to three sentences citing the evidence as path:line from your read_file output."
                }
            },
            "required": ["keep", "rationale"]
        }),
    }
}

/// System prompt of the collector. Engine tests tell collector calls apart by its opening.
const COLLECTOR_SYSTEM: &str = "You gather evidence for one team-rule violation that an earlier \
check reported on a pull request. <rule> is the team's rule, <candidate> says where the change \
is said to break it and how, and <diff> is that file's change.

You can read the repository at the pull request's head commit with read_file, grep, list_dir \
and find_files. Open the flagged line with read_file first, then read whatever else decides the \
claim: the surrounding function, a helper or definition it relies on, or a place that shows the \
rule is already met. Make every call answer a specific question; your steps are few.

Then call submit_evidence:
- keep: false only when code you read shows the rule is not broken (the claim misreads the \
code, or the rule does not apply here); otherwise true.
- confidence: \"high\" only when the lines you cite settle it.
- rationale: cite the lines you rely on as path:line, exactly as numbered in your read_file \
output.
Your last step offers no tools: if you reach it, reply with only that JSON object.";

/// System prompt of the judge. Engine tests tell judge calls apart by its opening.
const JUDGE_SYSTEM: &str = "You judge one team-rule violation from collected evidence. <rule> \
is the team's rule and <candidate> the reported violation. <excerpts> holds the repository \
lines read while checking it, each row as `<number>: <code>`; <diffs> shows the pull request's \
changes. Judge only from what these show.

Answer with one JSON object and nothing else:
{\"verdict\": \"violated\" | \"satisfied\" | \"not_applicable\" | \"insufficient_evidence\", \
\"path\": \"<file>\", \"line\": <number>, \"rationale\": \"<one to three sentences>\"}
- violated: the code plainly breaks the rule as written. `path` and `line` must name a row of \
<excerpts> that shows the violation.
- satisfied: the code meets the rule.
- not_applicable: the rule does not cover this code.
- insufficient_evidence: the excerpts do not settle it.";

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use adus_core::types::Severity;
    use adus_git::FileStatus;
    use adus_llm::ChatResponse;
    use adus_llm::router::ChatError;

    use super::*;
    use crate::test_support::{ScriptedChat, call, rule, text_turn, tools_turn, user_text};
    use crate::workspace::RepoWorkspace;

    const FAST_MODEL: &str = "fast-model";
    const FILE_A: &str = "fn alpha(input: Option<u32>) -> u32 {\n    let fallback = 0;\n    \
        // the value is always present here\n    let value = input.unwrap();\n    \
        value + fallback\n}\n\nfn beta() {}\n\nfn gamma() {}\n";

    fn file_a() -> FileDiff {
        FileDiff {
            path: "src/a.rs".into(),
            old_path: None,
            status: FileStatus::Modified,
            patch: Some(
                "@@ -1,4 +1,5 @@\n fn alpha(input: Option<u32>) -> u32 {\n     let fallback = 0;\n     // the value is always present here\n+    let value = input.unwrap();\n     value + fallback\n"
                    .into(),
            ),
        }
    }

    fn candidate() -> RawShardViolation {
        RawShardViolation {
            rule_index: 0,
            line: 4,
            start_line: None,
            claim: "`input.unwrap()` breaks the no-unwrap rule".into(),
            cited_line: None,
        }
    }

    fn read_a() -> Result<ChatResponse, ChatError> {
        tools_turn(vec![call(
            "r1",
            "read_file",
            json!({"path": "src/a.rs", "start_line": 1, "end_line": 10}),
        )])
    }

    fn submit(keep: bool, confidence: &str, rationale: &str) -> Result<ChatResponse, ChatError> {
        tools_turn(vec![call(
            "s1",
            SUBMIT_EVIDENCE,
            json!({"keep": keep, "confidence": confidence, "rationale": rationale}),
        )])
    }

    async fn verify(chat: &ScriptedChat) -> EvidenceVerdict {
        let toolbox = Toolbox::new(Arc::new(RepoWorkspace::from_files(&[(
            "src/a.rs",
            FILE_A.as_bytes(),
        )])));
        let usage = UsageContext::default();
        let ctx = RuleEvidenceContext {
            chat,
            provider_id: Uuid::from_u128(1),
            fast_model: FAST_MODEL,
            usage: &usage,
            token_budget: None,
            toolbox: &toolbox,
        };
        let rule = rule("No unwrap", Severity::Warning, None, true);
        let file = file_a();
        verify_rule_candidate(
            ctx,
            &rule,
            &file,
            &candidate(),
            std::slice::from_ref(&file),
            Instant::now() + Duration::from_secs(30),
        )
        .await
    }

    #[tokio::test]
    async fn evidence_judge_accepts_verified_citation_and_rejects_unverified() {
        let cases = [
            (
                r#"Verdict: {"verdict": "violated", "path": "src/a.rs", "line": "4", "rationale": "src/a.rs:4 calls unwrap"}"#,
                EvidenceVerdict::Verified {
                    path: "src/a.rs".into(),
                    line: 4,
                    rationale: "src/a.rs:4 calls unwrap".into(),
                },
            ),
            (
                r#"{"verdict": "violated", "path": "src/other.rs", "line": 12, "rationale": "src/other.rs:12"}"#,
                EvidenceVerdict::UnsupportedCitation,
            ),
            (
                r#"{"verdict": "satisfied", "path": "src/a.rs", "line": 4, "rationale": "fine"}"#,
                EvidenceVerdict::Rejected("satisfied"),
            ),
        ];
        for (judge_reply, expected) in cases {
            let chat = ScriptedChat::new(vec![
                read_a(),
                submit(true, "medium", "src/a.rs:4 calls unwrap"),
                text_turn(judge_reply),
            ]);
            assert_eq!(verify(&chat).await, expected, "judge reply {judge_reply}");

            let requests = chat.requests();
            assert_eq!(requests.len(), 3);
            assert!(
                requests[0]
                    .system
                    .starts_with("You gather evidence for one team-rule violation")
            );
            let judge = &requests[2];
            assert!(
                judge
                    .system
                    .starts_with("You judge one team-rule violation from collected evidence")
            );
            assert!(judge.tools.is_empty());
            assert_eq!(judge.model, FAST_MODEL);
            assert_eq!(judge.effort, Some(ReasoningEffort::High));
            assert_eq!(judge.max_tokens, RULE_JUDGE_MAX_TOKENS);
            // The judge sees the excerpt the collector read in its first step.
            assert!(user_text(judge).contains("4:     let value = input.unwrap();"));
            assert!(user_text(judge).contains("=== src/a.rs (modified) ==="));
        }
    }

    #[tokio::test]
    async fn collector_high_confidence_refutation_skips_judge() {
        let chat = ScriptedChat::new(vec![
            read_a(),
            submit(false, "high", "Guard at src/a.rs:4 handles it"),
        ]);

        assert_eq!(verify(&chat).await, EvidenceVerdict::Refuted);
        assert_eq!(chat.requests().len(), 2);
    }

    #[test]
    fn evidence_judge_rejects_violated_verdict_without_excerpt_line() {
        let excerpts = [ReadExcerpt {
            path: "src/a.rs".into(),
            content: "[src/a.rs: lines 3-5 of 9]\n3: let a = 1;\n4:   \n5: let b = a;".into(),
        }];
        let cases = [
            ("src/a.rs", 3, true),
            ("/src/a.rs", 5, true),
            ("./src/a.rs", 3, true),
            ("src/a.rs", 4, false),
            ("src/b.rs", 3, false),
            ("src/a.rs", 6, false),
            ("src/a.rs", 0, false),
        ];
        for (path, line, expected) in cases {
            assert_eq!(
                read_contains_line(&excerpts, path, line),
                expected,
                "{path}:{line}"
            );
        }
    }
}
