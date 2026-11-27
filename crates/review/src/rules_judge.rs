//! Sharded team rules: one cheap call per changed file judges the rules matching that file, and
//! every violation it claims then goes through the evidence loop (`rules_evidence.rs`). Only
//! violations the loop verifies become findings; they skip the Verifier. The engine runs this
//! next to the Finder when the repository tools are available.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use adus_core::types::normalize_path;
use adus_db::rules::{Rule, rule_matches_path};
use adus_git::{FileDiff, FileStatus};
use adus_llm::ReasoningEffort;
use futures::{StreamExt, stream};
use serde_json::Value;
use tokio::time::Instant;
use tracing::Instrument;

use crate::agent::{AgentLlm, chat_single_turn};
use crate::finder::FinderPass;
use crate::parse::{CommentableRange, Suggestion, array_arg, parse_u32_field};
use crate::prompt::{diff_block, severity_name, wrap};
use crate::redact::redact_secrets;
use crate::rules_evidence::{EvidenceVerdict, RuleEvidenceContext, verify_rule_candidate};
use crate::specialists::SpecialistKind;
use crate::verifier::CandidateFinding;

/// Shard calls in flight at once.
const SHARD_CONCURRENCY: usize = 4;
/// Output tokens of a shard call (the client adds the `High` reasoning reserve).
const SHARD_MAX_TOKENS: u32 = 2_048;
/// A head file longer than this many lines is not inlined in its shard.
const RULE_FILE_MAX_LINES: usize = 600;
/// A head file longer than this many characters is not inlined in its shard.
const RULE_FILE_MAX_CHARS: usize = 30_000;
/// Evidence loops in flight at once.
const RULE_EVIDENCE_CONCURRENCY: usize = 4;
/// The evidence loops must be done this long before the run's deadline.
const RULE_EVIDENCE_DEADLINE_RESERVE: Duration = Duration::from_secs(10);
/// Effort of a shard call (spec §2: fast @ High).
const SHARD_EFFORT: ReasoningEffort = ReasoningEffort::High;
/// Trace label of a shard call.
const RULE_SHARD_CALL: &str = "rule_shard";

/// System prompt of a shard call. The engine tests tell shard calls apart by its opening.
const SHARD_SYSTEM: &str = "You check one changed file against the team's rules. \
You get the numbered rules that apply to this file, the file's diff, and sometimes the whole \
file as it is after the change. Judge only those rules, and only against the lines this change \
adds or edits: code the change leaves untouched is not your concern, unless the change makes it \
break a rule (for example an import the change leaves unused).

Report a violation only when the code plainly breaks the rule as written. Do not report style \
taste, bugs unrelated to a rule, or a rule the code satisfies. When in doubt, report nothing: \
an evidence check runs on everything you report.

In the diff, each \"+\" and \" \" row starts with its line number in the new file; use those \
numbers. In <file_content>, each row starts with `<number>: `.

Answer with one JSON object and nothing else:
{\"violations\": [{\"rule_id\": <the number of the rule in brackets>, \"line\": <new-file line \
number>, \"start_line\": <optional first line of a multi-line violation>, \"claim\": \"<which \
rule is broken and how, naming the code>\"}]}
Use an empty list when no rule is violated. `line` should be a \"+\" row of the diff.";

/// A rule violation claimed by a shard call, anchored to a line of the file's diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RawShardViolation {
    /// Zero-based index into the `matching_rules` slice given to [`build_shard_prompts`].
    pub rule_index: usize,
    /// A `+` line of the file's diff (new-file numbering).
    pub line: u32,
    /// First line of a multi-line violation, always before `line` in the same hunk.
    pub start_line: Option<u32>,
    pub claim: String,
    /// The line the model cited when it was not a `+` line and `line` was moved to the nearest
    /// one; `None` when `line` is what the model cited.
    pub cited_line: Option<u32>,
}

/// `(system, user)` prompts that judge `matching_rules` against one changed file. Rules are
/// numbered `[1]..[n]` in slice order, which is what `rule_id` in the reply refers to.
/// `head_content` is the file after the change, already numbered and redacted; the diff is
/// passed as it is, so a secret the PR adds can be reported.
pub(crate) fn build_shard_prompts(
    file: &FileDiff,
    matching_rules: &[&Rule],
    head_content: Option<&str>,
) -> (String, String) {
    let rules: Vec<String> = matching_rules
        .iter()
        .enumerate()
        .map(|(i, rule)| {
            format!(
                "[{}] ({}) {}\n{}",
                i + 1,
                severity_name(rule.severity),
                rule.title,
                rule.body
            )
        })
        .collect();
    let mut sections = vec![
        wrap("rules", &rules.join("\n\n")),
        wrap("diff", &diff_block(file)),
    ];
    if let Some(content) = head_content {
        sections.push(wrap("file_content", content));
    }
    (SHARD_SYSTEM.to_string(), sections.join("\n\n"))
}

/// Violations of a shard reply, with `rule_id`s and lines checked against `rule_count` and the
/// file's diff. Never fails: a reply without a list yields nothing and malformed entries are
/// skipped. Accepts `{"violations": [...]}`, `{"findings": [...]}` or a bare array (a string
/// holding the array too), fenced or surrounded by prose, with numbers possibly quoted.
///
/// A `line` that is a `+` line is kept, with its `start_line` only when both lie in one hunk. A
/// `line` on another line of a hunk (a context line) moves to the nearest `+` line of that hunk,
/// the earlier one on a tie, losing its `start_line` and recording the cited line. A `line`
/// outside every hunk of `range`, or in a hunk without a `+` line, drops the entry.
pub(crate) fn parse_shard_response(
    raw: &str,
    rule_count: usize,
    range: &CommentableRange,
) -> Vec<RawShardViolation> {
    shard_items(raw)
        .unwrap_or_default()
        .iter()
        .filter_map(|item| to_violation(item, rule_count, range))
        .collect()
}

/// The first list of violation objects in `raw`. An empty list counts only when no list has
/// entries, so a `[]` in the lead-in prose cannot hide the real one.
// ponytail: O(n²) on text with many `{` or `[` that are not JSON; model replies are small.
fn shard_items(raw: &str) -> Option<Vec<Value>> {
    let mut saw_empty = false;
    for (start, _) in raw.match_indices(['{', '[']) {
        let Some(Ok(value)) = serde_json::Deserializer::from_str(&raw[start..])
            .into_iter::<Value>()
            .next()
        else {
            continue;
        };
        let items = match &value {
            Value::Object(_) => {
                match array_arg(&value, "violations").or_else(|| array_arg(&value, "findings")) {
                    Some(items) => items.into_owned(),
                    None => continue,
                }
            }
            Value::Array(items) if items.iter().all(Value::is_object) => items.clone(),
            _ => continue,
        };
        if !items.is_empty() {
            return Some(items);
        }
        saw_empty = true;
    }
    saw_empty.then(Vec::new)
}

fn to_violation(
    item: &Value,
    rule_count: usize,
    range: &CommentableRange,
) -> Option<RawShardViolation> {
    let rule_id = usize::try_from(parse_u32_field(item.get("rule_id")?)?).ok()?;
    if rule_id == 0 || rule_id > rule_count {
        return None;
    }
    let claim = item.get("claim")?.as_str()?.trim();
    if claim.is_empty() {
        return None;
    }
    let cited = parse_u32_field(item.get("line")?)?;
    let start_line = item.get("start_line").and_then(parse_u32_field);
    let (line, start_line, cited_line) = anchor(cited, start_line, range)?;
    Some(RawShardViolation {
        rule_index: rule_id - 1,
        line,
        start_line,
        claim: claim.to_string(),
        cited_line,
    })
}

/// `(line, start_line, cited_line)` for a cited line, or `None` when it cannot be anchored.
fn anchor(
    cited: u32,
    start_line: Option<u32>,
    range: &CommentableRange,
) -> Option<(u32, Option<u32>, Option<u32>)> {
    if range.lines.contains(&cited) {
        let start = start_line.filter(|&start| range.spans_one_hunk(start, cited));
        return Some((cited, start, None));
    }
    let hunk = range.hunks.iter().find(|hunk| hunk.contains(&cited))?;
    let nearest = range
        .lines
        .range(hunk.clone())
        .min_by_key(|&&line| (line.abs_diff(cited), line))?;
    Some((*nearest, None, Some(cited)))
}

/// What the Sharded Rules produced for one review.
#[derive(Debug, Default)]
pub(crate) struct ShardedRulesOutput {
    /// Verified violations, ready for Semantic Dedup (`needs_verify()` is false).
    pub findings: Vec<CandidateFinding>,
    /// Shard calls started: changed files with at least one matching enabled rule.
    pub shards_run: usize,
    /// Violations the shards claimed, each sent to the evidence loop.
    pub candidates: usize,
    /// Violations the evidence loop verified.
    pub verified: usize,
}

/// Judges every changed file of `files` that has a commentable line in `allowed` (keyed by
/// normalized path) and at least one matching enabled rule of `rules`, then runs the evidence
/// loop on each violation the shards claim. Shards run [`SHARD_CONCURRENCY`] at a time by
/// `deadline`; evidence loops [`RULE_EVIDENCE_CONCURRENCY`] at a time by `deadline` minus
/// [`RULE_EVIDENCE_DEADLINE_RESERVE`]. A shard or a violation that fails or runs out of time is
/// dropped (and logged at debug); it never fails the review.
pub(crate) async fn run_sharded_rules(
    ctx: RuleEvidenceContext<'_>,
    files: &[FileDiff],
    allowed: &HashMap<String, CommentableRange>,
    rules: &[Rule],
    deadline: Instant,
) -> ShardedRulesOutput {
    let shards: Vec<(&FileDiff, &CommentableRange, Vec<&Rule>)> = files
        .iter()
        .filter_map(|file| {
            let range = allowed
                .get(&normalize_path(&file.path))
                .filter(|range| !range.is_empty())?;
            let matching: Vec<&Rule> = rules
                .iter()
                .filter(|rule| rule.enabled && rule_matches_path(rule, &file.path))
                .collect();
            (!matching.is_empty()).then_some((file, range, matching))
        })
        .collect();
    let shards_run = shards.len();

    // Futures are built up front, as in `finder.rs`, so the stream's future stays `Send`.
    let shard_runs: Vec<_> = shards
        .into_iter()
        .map(|(file, range, matching)| async move {
            run_shard(ctx, file, range, &matching, deadline)
                .await
                .into_iter()
                .filter_map(|violation| {
                    let rule = *matching.get(violation.rule_index)?;
                    Some((file, rule, violation))
                })
                .collect::<Vec<_>>()
        })
        .collect();
    let claimed: Vec<(&FileDiff, &Rule, RawShardViolation)> = stream::iter(shard_runs)
        .buffered(SHARD_CONCURRENCY)
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .flatten()
        .collect();
    let candidates = claimed.len();

    let evidence_deadline = deadline
        .checked_sub(RULE_EVIDENCE_DEADLINE_RESERVE)
        .unwrap_or(deadline);
    let checks: Vec<_> = claimed
        .iter()
        .map(|(file, rule, violation)| async move {
            if Instant::now() >= evidence_deadline {
                return EvidenceVerdict::Rejected("no_time_left");
            }
            verify_rule_candidate(ctx, rule, file, violation, files, evidence_deadline).await
        })
        .collect();
    let verdicts: Vec<EvidenceVerdict> = stream::iter(checks)
        .buffered(RULE_EVIDENCE_CONCURRENCY)
        .collect()
        .await;

    let mut findings = Vec::new();
    for ((file, rule, violation), verdict) in claimed.iter().zip(verdicts) {
        match verdict {
            EvidenceVerdict::Verified {
                path,
                line,
                rationale,
            } => findings.push(rule_finding(rule, file, violation, &path, line, &rationale)),
            other => tracing::debug!(
                rule = %rule.title,
                file = %file.path,
                line = violation.line,
                reason = ?other,
                "rule violation dropped"
            ),
        }
    }
    ShardedRulesOutput {
        verified: findings.len(),
        findings,
        shards_run,
        candidates,
    }
}

/// One shard call: the violations of `matching` it claims in `file`, empty on any failure.
async fn run_shard(
    ctx: RuleEvidenceContext<'_>,
    file: &FileDiff,
    range: &CommentableRange,
    matching: &[&Rule],
    deadline: Instant,
) -> Vec<RawShardViolation> {
    let span = tracing::debug_span!("rule_shard", file = %file.path, rules = matching.len());
    async {
        if ctx.token_budget.is_some_and(|budget| budget.exhausted()) {
            tracing::debug!("token budget exhausted; rule shard not started");
            return Vec::new();
        }
        let judged = tokio::time::timeout_at(deadline, async {
            // An added file is whole in its diff already.
            let head = match file.status {
                FileStatus::Added => None,
                _ => {
                    ctx.toolbox
                        .read_head_file_capped(&file.path, RULE_FILE_MAX_LINES, RULE_FILE_MAX_CHARS)
                        .await
                }
            };
            let (system, user) = build_shard_prompts(file, matching, head.as_deref());
            let llm = AgentLlm {
                chat: ctx.chat,
                provider_id: ctx.provider_id,
                model: ctx.fast_model,
                system: &system,
                effort: Some(SHARD_EFFORT),
                usage: ctx.usage,
            };
            chat_single_turn(
                &llm,
                RULE_SHARD_CALL,
                ctx.token_budget.map(Arc::as_ref),
                user,
                SHARD_MAX_TOKENS,
            )
            .await
        })
        .await;
        match judged {
            Ok(Ok(reply)) => {
                let violations = parse_shard_response(&reply, matching.len(), range);
                tracing::debug!(violations = violations.len(), "rule shard judged");
                violations
            }
            Ok(Err(err)) => {
                tracing::debug!(error = %err, "rule shard failed");
                Vec::new()
            }
            Err(_) => {
                tracing::debug!("rule shard cut by the deadline");
                Vec::new()
            }
        }
    }
    .instrument(span)
    .await
}

/// The finding of a verified violation, anchored on the violation's `+` line in `file`. The
/// judge's citation (`path:line`, possibly another file) is named in the body only.
fn rule_finding(
    rule: &Rule,
    file: &FileDiff,
    violation: &RawShardViolation,
    path: &str,
    line: u32,
    rationale: &str,
) -> CandidateFinding {
    let mut body = format!(
        "Rule \"{}\": {} (verified at {path}:{line}: {rationale})",
        rule.title, violation.claim
    );
    if let Some(cited) = violation.cited_line {
        body.push_str(&format!(" (line {cited}, outside the changed lines)"));
    }
    CandidateFinding {
        suggestion: Suggestion {
            file: normalize_path(&file.path),
            start_line: violation.start_line,
            line: violation.line,
            severity: Some(rule.severity),
            category: None,
            title: String::new(),
            body: redact_secrets(&body).into_owned(),
            suggestion: None,
        },
        specialist: SpecialistKind::Generalist,
        investigated_by_finder: false,
        origin: FinderPass::ShardedRules,
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use adus_core::types::Severity;
    use adus_git::FileStatus;
    use adus_llm::router::UsageContext;
    use serde_json::json;
    use uuid::Uuid;

    use super::*;
    use crate::test_support::{ScriptedChat, call, rule, text_turn, tools_turn, user_text};
    use crate::tools::Toolbox;
    use crate::workspace::RepoWorkspace;

    /// `+` lines 2, 10 and 12; hunks 1-4, 10-13 and 30-32 (the last one has no `+` line).
    const PATCH: &str = "@@ -1,3 +1,4 @@\n c1\n+a2\n c3\n c4\n\
                         @@ -9,3 +10,4 @@\n+b10\n c11\n+b12\n c13\n\
                         @@ -30,4 +30,3 @@\n x30\n-old\n x31\n x32\n";

    fn violation(
        rule_index: usize,
        line: u32,
        start_line: Option<u32>,
        cited_line: Option<u32>,
    ) -> RawShardViolation {
        RawShardViolation {
            rule_index,
            line,
            start_line,
            claim: "c".into(),
            cited_line,
        }
    }

    #[test]
    fn lenient_shard_parser_handles_bare_array_string_lines_and_invalid_anchors() {
        let range = CommentableRange::from_patch(PATCH);
        let parse = |raw: &str| parse_shard_response(raw, 3, &range);

        // Accepted envelopes: `violations`, `findings`, a bare array, a string holding the list,
        // all with prose or a code fence around them.
        let cases = [
            r#"{"violations": [{"rule_id": 1, "line": 2, "claim": "c"}]}"#,
            r#"{"findings": [{"rule_id": 1, "line": 2, "claim": "c"}]}"#,
            r#"[{"rule_id": 1, "line": 2, "claim": "c"}]"#,
            r#"{"violations": "[{\"rule_id\": 1, \"line\": 2, \"claim\": \"c\"}]"}"#,
            "Here you go:\n```json\n[{\"rule_id\": \"1\", \"line\": \"2\", \"claim\": \"c\"}]\n```\nDone.",
        ];
        for raw in cases {
            assert_eq!(parse(raw), [violation(0, 2, None, None)], "{raw}");
        }
        assert!(parse("[]").is_empty());
        assert!(parse(r#"{"violations": []}"#).is_empty());
        assert!(parse("no violations").is_empty());

        // `start_line` survives only inside the hunk of `line`.
        let got = parse(
            r#"[{"rule_id": 2, "start_line": 10, "line": 12, "claim": "c"},
                {"rule_id": 3, "start_line": 2, "line": 12, "claim": "c"}]"#,
        );
        assert_eq!(
            got,
            [
                violation(1, 12, Some(10), None),
                violation(2, 12, None, None)
            ]
        );

        // Invalid rule ids, off-diff lines, missing claims and a hunk without `+` are dropped.
        let got = parse(
            r#"[{"rule_id": 0, "line": 2, "claim": "c"},
                {"rule_id": 99, "line": 2, "claim": "c"},
                {"rule_id": 4, "line": 2, "claim": "c"},
                {"rule_id": 1, "line": 999, "claim": "c"},
                {"rule_id": 1, "line": 31, "claim": "c"},
                {"rule_id": 1, "line": 2, "claim": "  "},
                {"rule_id": 1, "line": 2}]"#,
        );
        assert!(got.is_empty(), "{got:?}");

        // A context line moves to the nearest `+` line of its hunk (earlier one on a tie), loses
        // its `start_line` and is remembered in `cited_line`.
        let got = parse(
            r#"[{"rule_id": 1, "line": 4, "start_line": 1, "claim": "c"},
                {"rule_id": 1, "line": 11, "claim": "c"},
                {"rule_id": 1, "line": 13, "claim": "c"}]"#,
        );
        assert_eq!(
            got,
            [
                violation(0, 2, None, Some(4)),
                violation(0, 10, None, Some(11)),
                violation(0, 12, None, Some(13)),
            ]
        );
    }

    #[test]
    fn shard_prompts_number_rules_and_wrap_diff_and_head_content() {
        let file = FileDiff {
            path: "src/a.rs".into(),
            old_path: None,
            status: FileStatus::Modified,
            patch: Some(PATCH.into()),
        };
        let first = rule("No unwrap", Severity::Critical, Some("**/*.rs"), true);
        let second = rule("Doc comments", Severity::Info, None, true);
        let rules = [&first, &second];

        let (system, user) = build_shard_prompts(&file, &rules, Some("1: fn a() {}"));
        assert!(system.starts_with("You check one changed file against the team's rules"));
        let rules_at = user.find("[1] (critical) No unwrap").expect("rule 1");
        assert!(rules_at < user.find("[2] (info) Doc comments").expect("rule 2"));
        assert!(user.contains("body of No unwrap"));
        assert!(user.contains("=== src/a.rs (modified) ==="));
        assert!(user.contains("<file_content>\n1: fn a() {}\n</file_content>"));

        let (_, user) = build_shard_prompts(&file, &rules, None);
        assert!(!user.contains("<file_content>"));
    }

    #[tokio::test]
    async fn run_sharded_rules_evaluates_matching_files_and_skips_unmatched() {
        let added = FileDiff {
            path: "src/a.rs".into(),
            old_path: None,
            status: FileStatus::Added,
            patch: Some("@@ -0,0 +1,3 @@\n+fn a() {\n+    x.unwrap();\n+}\n".into()),
        };
        let docs = FileDiff {
            path: "docs/b.md".into(),
            old_path: None,
            status: FileStatus::Modified,
            patch: Some("@@ -1,1 +1,2 @@\n a\n+b\n".into()),
        };
        let files = [added, docs];
        let allowed: HashMap<String, CommentableRange> = files
            .iter()
            .map(|file| {
                let range = CommentableRange::from_patch(file.patch.as_deref().unwrap_or(""));
                (file.path.clone(), range)
            })
            .collect();
        // `src/a.rs` is readable in the clone: only its `Added` status keeps it out of the shard.
        let toolbox = Toolbox::new(Arc::new(RepoWorkspace::from_files(&[
            ("src/a.rs", b"fn a() {\n    x.unwrap();\n}\n"),
            ("docs/b.md", b"a\nb\n"),
        ])));
        let chat = ScriptedChat::new(vec![
            text_turn(
                r#"{"violations": [{"rule_id": 1, "line": 2, "claim": "`x.unwrap()` panics"}]}"#,
            ),
            tools_turn(vec![call("r1", "read_file", json!({"path": "src/a.rs"}))]),
            tools_turn(vec![call(
                "s1",
                "submit_evidence",
                json!({"keep": true, "confidence": "high", "rationale": "src/a.rs:2 unwraps"}),
            )]),
            text_turn(
                r#"{"verdict": "violated", "path": "src/a.rs", "line": 2, "rationale": "it calls unwrap"}"#,
            ),
        ]);
        let usage = UsageContext::default();
        let ctx = RuleEvidenceContext {
            chat: &chat,
            provider_id: Uuid::from_u128(1),
            fast_model: "fast-model",
            usage: &usage,
            token_budget: None,
            toolbox: &toolbox,
        };
        let rules = [rule("No unwrap", Severity::Critical, Some("src/**"), true)];

        let out = run_sharded_rules(
            ctx,
            &files,
            &allowed,
            &rules,
            Instant::now() + Duration::from_secs(60),
        )
        .await;

        assert_eq!((out.shards_run, out.candidates, out.verified), (1, 1, 1));
        let requests = chat.requests();
        let shards: Vec<_> = requests
            .iter()
            .filter(|r| {
                r.system
                    .starts_with("You check one changed file against the team's rules")
            })
            .collect();
        assert_eq!(shards.len(), 1, "docs/b.md matches no rule");
        let shard = user_text(shards[0]);
        assert!(shard.contains("=== src/a.rs (added) ==="));
        assert!(!shard.contains("docs/b.md"));
        assert!(
            !shard.contains("<file_content>"),
            "an added file is all in its diff"
        );
        assert_eq!(shards[0].model, "fast-model");
        assert_eq!(shards[0].effort, Some(ReasoningEffort::High));
        assert_eq!(shards[0].max_tokens, SHARD_MAX_TOKENS);

        let [finding] = out.findings.as_slice() else {
            panic!("one verified finding: {:?}", out.findings);
        };
        assert!(!finding.needs_verify());
        assert_eq!(finding.origin, FinderPass::ShardedRules);
        let s = &finding.suggestion;
        assert_eq!(
            (s.file.as_str(), s.line, s.start_line),
            ("src/a.rs", 2, None)
        );
        assert_eq!(s.severity, Some(Severity::Critical));
        assert_eq!(s.category, None);
        assert_eq!(
            s.body,
            "Rule \"No unwrap\": `x.unwrap()` panics (verified at src/a.rs:2: it calls unwrap)"
        );
    }
}
