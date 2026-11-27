//! Sharded team rules: one cheap call per changed file judges the rules matching that file.
//! This module holds the prompt builder and the lenient parser of the reply; the model call and
//! the evidence loop live elsewhere.

use std::ops::RangeInclusive;

use adus_db::rules::Rule;
use adus_git::FileDiff;
use adus_git::diff::same_hunk;
use serde_json::Value;

use crate::parse::{CommentableRange, array_arg, parse_u32_field};
use crate::prompt::{diff_block, severity_name, wrap};

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
/// outside every hunk, or in a hunk without a `+` line, drops the entry. `hunks` are the file's
/// right-side hunk ranges, context included.
pub(crate) fn parse_shard_response(
    raw: &str,
    rule_count: usize,
    range: &CommentableRange,
    hunks: &[RangeInclusive<u32>],
) -> Vec<RawShardViolation> {
    shard_items(raw)
        .unwrap_or_default()
        .iter()
        .filter_map(|item| to_violation(item, rule_count, range, hunks))
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
    hunks: &[RangeInclusive<u32>],
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
    let (line, start_line, cited_line) = anchor(cited, start_line, range, hunks)?;
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
    hunks: &[RangeInclusive<u32>],
) -> Option<(u32, Option<u32>, Option<u32>)> {
    if range.lines.contains(&cited) {
        let start = start_line.filter(|&start| same_hunk(hunks, start, cited));
        return Some((cited, start, None));
    }
    let hunk = hunks.iter().find(|hunk| hunk.contains(&cited))?;
    let nearest = range
        .lines
        .range(hunk.clone())
        .min_by_key(|&&line| (line.abs_diff(cited), line))?;
    Some((*nearest, None, Some(cited)))
}

#[cfg(test)]
mod tests {
    use adus_core::types::Severity;
    use adus_git::FileStatus;

    use super::*;
    use crate::test_support::rule;

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
        let parse = |raw: &str| parse_shard_response(raw, 3, &range, &range.hunks);

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
}
