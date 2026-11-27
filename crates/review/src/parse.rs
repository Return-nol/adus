//! Parsing dan validasi jawaban LLM menjadi daftar temuan review.

use std::borrow::Cow;
use std::collections::{BTreeSet, HashMap};
use std::ops::RangeInclusive;

use adus_core::types::{Severity, normalize_path};
use adus_git::diff::{commentable_hunks, commentable_lines, same_hunk};
use adus_llm::ToolDef;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};
use thiserror::Error;

/// Which lens a finding belongs to. The generalist names it; a specialist's findings default to
/// its own (`SpecialistKind::category`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FindingCategory {
    Bug,
    Security,
    Performance,
    /// Breaks a team rule or a repository instruction.
    RepoInstruction,
}

impl FindingCategory {
    /// `"bug"`, `"security"`, `"performance"` or `"repo_instruction"`, ignoring case and
    /// surrounding whitespace.
    fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "bug" => Some(Self::Bug),
            "security" => Some(Self::Security),
            "performance" => Some(Self::Performance),
            "repo_instruction" => Some(Self::RepoInstruction),
            _ => None,
        }
    }
}

/// Satu temuan dari LLM untuk satu baris (atau rentang baris) di file baru.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Suggestion {
    pub file: String,
    /// First line of the span `start_line..=line` of existing new-file lines the finding (and its
    /// `suggestion`) covers. `None`, or a value equal to `line` (which parsing turns into `None`),
    /// means a single-line finding.
    pub start_line: Option<u32>,
    pub line: u32,
    /// `None` until classified: the lean Finder output carries none, and `classify_severity`
    /// fills it after verification.
    pub severity: Option<Severity>,
    /// `None` when the model named no valid category and its Finder role implies none.
    pub category: Option<FindingCategory>,
    /// Empty for lean Finder output until `enrich_suggestions` sets it after verification.
    pub title: String,
    pub body: String,
    /// Kode pengganti untuk baris/rentang itu (opsional).
    pub suggestion: Option<String>,
}

#[cfg(test)]
impl AsRef<Suggestion> for Suggestion {
    fn as_ref(&self) -> &Suggestion {
        self
    }
}

#[cfg(test)]
impl AsMut<Suggestion> for Suggestion {
    fn as_mut(&mut self) -> &mut Suggestion {
        self
    }
}

/// Baris dan rentang hunk sisi kanan pada satu file yang valid untuk dikomentari.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct CommentableRange {
    pub lines: BTreeSet<u32>,
    pub hunks: Vec<RangeInclusive<u32>>,
}

impl CommentableRange {
    pub(crate) fn from_patch(patch: &str) -> Self {
        Self {
            lines: commentable_lines(patch),
            hunks: commentable_hunks(patch),
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// Whether `start_line < line` and both lie in one diff hunk.
    pub(crate) fn spans_one_hunk(&self, start_line: u32, line: u32) -> bool {
        same_hunk(&self.hunks, start_line, line)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("no JSON array found in the model output")]
pub(crate) struct ParseError;

/// Ambil array JSON pertama dari jawaban model (boleh dibungkus ```json atau diapit prosa).
/// Objek yang kurang field wajib (`file`, `line`, `body`) dilewati, bukan menggagalkan parse.
pub(crate) fn parse_suggestions(raw: &str) -> Result<Vec<Suggestion>, ParseError> {
    let items = first_json_array(raw).ok_or(ParseError)?;
    Ok(items.iter().filter_map(to_suggestion).collect())
}

/// Coba parse dari setiap `[`: ambil array objek pertama yang TIDAK kosong. Array kosong hanya
/// dipakai jika tidak ada yang berisi, supaya `String[] args` atau `- [ ]` di prosa pembuka tidak
/// menelan array temuan yang asli. Teks setelah array diabaikan. Syarat "semua elemen objek"
/// mencegah `[1]` di prosa ikut terambil.
// ponytail: O(n²) di teks dengan banyak `[` yang bukan JSON; jawaban LLM kecil, jadi aman.
fn first_json_array(raw: &str) -> Option<Vec<Map<String, Value>>> {
    let mut saw_empty = false;
    for (start, _) in raw.match_indices('[') {
        let parsed = serde_json::Deserializer::from_str(&raw[start..])
            .into_iter::<Vec<Map<String, Value>>>()
            .next();
        match parsed {
            Some(Ok(items)) if !items.is_empty() => return Some(items),
            Some(Ok(_)) => saw_empty = true,
            _ => {}
        }
    }
    saw_empty.then(Vec::new)
}

/// The first JSON object in `raw` (fenced or surrounded by prose) that `read` accepts.
/// Text after the object is ignored.
// ponytail: O(n²) on text with many `{` that are not JSON; model replies are small.
pub(crate) fn first_json_value<T>(
    raw: &str,
    mut read: impl FnMut(Value) -> Option<T>,
) -> Option<T> {
    raw.match_indices('{').find_map(|(start, _)| {
        let value = serde_json::Deserializer::from_str(&raw[start..])
            .into_iter::<Value>()
            .next()?
            .ok()?;
        read(value)
    })
}

/// The first JSON object in `raw` (fenced or surrounded by prose) that deserializes into `T`.
/// Objects that do not fit `T` (for example a quoted finding) are skipped; text after the
/// object is ignored.
// ponytail: O(n²) on text with many `{` that are not JSON; model replies are small.
pub(crate) fn first_json_object<T: DeserializeOwned>(raw: &str) -> Option<T> {
    raw.match_indices('{').find_map(|(start, _)| {
        serde_json::Deserializer::from_str(&raw[start..])
            .into_iter::<T>()
            .next()?
            .ok()
    })
}

/// Parses a JSON boolean or a `"true"` / `"false"` string (case- and whitespace-insensitive).
pub(crate) fn parse_bool_field(val: &Value) -> Option<bool> {
    match val {
        Value::Bool(flag) => Some(*flag),
        Value::String(text) => {
            let trimmed = text.trim();
            if trimmed.eq_ignore_ascii_case("true") {
                Some(true)
            } else if trimmed.eq_ignore_ascii_case("false") {
                Some(false)
            } else {
                None
            }
        }
        _ => None,
    }
}

pub(crate) fn parse_u32_field(val: &Value) -> Option<u32> {
    let n = match val {
        Value::Number(n) => n.as_u64()?,
        Value::String(s) => s.trim().parse().ok()?,
        _ => return None,
    };
    u32::try_from(n).ok()
}

/// `"critical"`, `"warning"` or `"info"`, ignoring case and surrounding whitespace.
pub(crate) fn parse_severity(text: &str) -> Option<Severity> {
    match text.trim().to_ascii_lowercase().as_str() {
        "critical" => Some(Severity::Critical),
        "warning" => Some(Severity::Warning),
        "info" => Some(Severity::Info),
        _ => None,
    }
}

/// One finding object, lean (`file`, `line`, `body`, optional `start_line` and `category`) or
/// legacy (with `title`, `severity` and `suggestion`). `None` without a non-blank `file`, a valid
/// `line`, or a `body`.
fn to_suggestion(item: &Map<String, Value>) -> Option<Suggestion> {
    let text = |key: &str| item.get(key)?.as_str().map(str::to_string);
    let line = parse_u32_field(item.get("line")?)?;
    // `range_ok == false`: the model asked for a range that cannot be valid (zero, after
    // `line`, not a number). Its suggestion was written for lines that will not be replaced, so
    // it goes too; an equal `start_line` is simply a single-line finding.
    let (start_line, range_ok) = match item.get("start_line") {
        None | Some(Value::Null) => (None, true),
        Some(value) => match parse_u32_field(value) {
            Some(start) if start == line => (None, true),
            Some(start) if start > 0 && start < line => (Some(start), true),
            _ => (None, false),
        },
    };
    let severity = text("severity").and_then(|s| parse_severity(&s));
    let file = text("file").filter(|f| !f.trim().is_empty())?;
    Some(Suggestion {
        file,
        start_line,
        line,
        severity,
        category: text("category").and_then(|c| FindingCategory::parse(&c)),
        title: text("title").unwrap_or_default(),
        body: text("body")?,
        suggestion: text("suggestion").filter(|_| range_ok),
    })
}

/// The Finder's terminal tool. Uses only `type`, `properties`, `required`, `description`,
/// `items` and `enum`, the JSON-schema subset every provider (Gemini included) accepts.
/// `review_log` is listed (and required) before `findings` so the model records its review
/// work first. It is a work record, never a request for the model's reasoning: Anthropic's
/// `reasoning_extraction` classifier declines requests that ask for that. `category` is required
/// from the generalist only; a specialist's findings default to its own category.
pub(crate) fn submit_findings_tool_def(is_generalist: bool) -> ToolDef {
    let finding_required = if is_generalist {
        json!(["file", "line", "category", "body"])
    } else {
        json!(["file", "line", "body"])
    };
    ToolDef {
        name: "submit_findings".into(),
        description: "Submit your final review and end it. Call it exactly once, after your \
            review_log covers every changed function and every lens, with every finding you \
            promoted, or with an empty list when nothing is worth reporting. List in \
            `dismissed` every suspicion you ruled out, with the code that rules it out."
            .into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "review_log": {
                    "type": "string",
                    "description": "Write this first. A record of the review work, never posted: for each changed function or hunk, the lenses applied, the callers, callees and files read, and each suspicion with its outcome (reported, or dismissed with the code that rules it out)."
                },
                "findings": {
                    "type": "array",
                    "description": "The findings you promoted; empty when there is nothing to report.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "file": {
                                "type": "string",
                                "description": "Path of the changed file exactly as shown in <diffs>."
                            },
                            "start_line": {
                                "type": "integer",
                                "description": "Optional. When the problem spans several lines, the first of them: a \"+\" or \" \" line in the same diff hunk, strictly less than `line`."
                            },
                            "line": {
                                "type": "integer",
                                "description": "New-file line number of a \"+\" line in that file's diff (the last line of the span when `start_line` is set)."
                            },
                            "category": {
                                "type": "string",
                                "enum": ["bug", "security", "performance", "repo_instruction"],
                                "description": "The lens the finding belongs to; repo_instruction when it breaks a repository instruction (or a team rule, when <rules> is given)."
                            },
                            "body": {
                                "type": "string",
                                "description": "The concrete trigger, the impact, and the code path (symbols and file paths) through which it fails, then the fix. Posted as written."
                            }
                        },
                        "required": finding_required
                    }
                },
                "dismissed": {
                    "type": "array",
                    "description": "Optional. Every suspicion you tested, under every lens, and ruled out. Never posted; kept only in Adus's logs.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "file": {
                                "type": "string",
                                "description": "Path of the changed file the suspicion was about."
                            },
                            "line": {
                                "type": "integer",
                                "description": "Optional new-file line number the suspicion was about."
                            },
                            "suspicion": {
                                "type": "string",
                                "description": "What could have gone wrong."
                            },
                            "why_safe": {
                                "type": "string",
                                "description": "What rules it out: the guard, type, constraint or caller you read."
                            }
                        },
                        "required": ["file", "suspicion", "why_safe"]
                    }
                }
            },
            "required": ["review_log", "findings"]
        }),
    }
}

/// `args[key]` as an array. Claude models often send an array argument as a JSON-encoded string
/// inside an otherwise valid tool call, so a string that parses to a JSON array is accepted too;
/// any other value (or string) yields `None`.
pub(crate) fn array_arg<'a>(args: &'a Value, key: &str) -> Option<Cow<'a, [Value]>> {
    match args.get(key)? {
        Value::Array(items) => Some(Cow::Borrowed(items.as_slice())),
        Value::String(text) => match serde_json::from_str(text) {
            Ok(Value::Array(items)) => Some(Cow::Owned(items)),
            _ => None,
        },
        _ => None,
    }
}

/// Most `dismissed` entries kept from one `submit_findings` call.
const MAX_DISMISSED: usize = 30;
/// Characters kept of a dismissal's `suspicion` and `why_safe`.
const DISMISSED_TEXT_CHARS: usize = 300;

/// A suspicion the Finder ruled out, from the optional `dismissed` list of `submit_findings`.
/// Only logged: never posted, persisted or shown to another stage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Dismissal {
    pub file: String,
    pub line: Option<u32>,
    pub suspicion: String,
    pub why_safe: String,
}

/// Reads the optional `dismissed` list (an array, or a string holding one). Never fails: a
/// missing or non-array list yields nothing, and malformed entries are skipped. Every text
/// (`file` included) is secret-redacted and then cut to [`DISMISSED_TEXT_CHARS`], so every
/// returned value is safe to log. Redacting first keeps a cut from splitting a secret into an
/// unrecognisable, partly logged prefix.
pub(crate) fn parse_dismissed(args: &Value) -> Vec<Dismissal> {
    let Some(items) = array_arg(args, "dismissed") else {
        return Vec::new();
    };
    fn text<'a>(item: &'a Value, key: &str) -> Option<&'a str> {
        item.get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
    }
    let clean = |s: &str| -> String {
        crate::redact::redact_secrets(s)
            .chars()
            .take(DISMISSED_TEXT_CHARS)
            .collect()
    };
    items
        .iter()
        .filter_map(|item| {
            let (file, suspicion, why_safe) = (
                text(item, "file")?,
                text(item, "suspicion")?,
                text(item, "why_safe")?,
            );
            Some(Dismissal {
                file: clean(file),
                line: item
                    .get("line")
                    .and_then(Value::as_u64)
                    .and_then(|line| u32::try_from(line).ok()),
                suspicion: clean(suspicion),
                why_safe: clean(why_safe),
            })
        })
        .take(MAX_DISMISSED)
        .collect()
}

/// The `review_log` of a `submit_findings` call, trimmed and secret-redacted so it is safe to
/// log; `None` when missing or blank. A model that still names it `reasoning` (the field's
/// earlier name, and Kodus's) is read the same way. Never posted, and its absence never refuses
/// a submission.
pub(crate) fn parse_review_log(args: &Value) -> Option<String> {
    ["review_log", "reasoning"]
        .into_iter()
        .find_map(|key| {
            args.get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
        })
        .map(|text| crate::redact::redact_secrets(text).into_owned())
}

/// Reads the `submit_findings` findings (an array, or a string holding one; `review_log` is not
/// needed). Elements that are not objects or lack a required field are skipped, as in
/// [`parse_suggestions`]. The error text is shown to the model.
pub(crate) fn parse_submit_findings(args: &Value) -> Result<Vec<Suggestion>, String> {
    let items = array_arg(args, "findings").ok_or_else(|| {
        "expected an object with a `findings` array of finding objects".to_string()
    })?;
    Ok(items
        .iter()
        .filter_map(Value::as_object)
        .filter_map(to_suggestion)
        .collect())
}

/// Validates that every finding in a `submit_findings` batch targets a file in `allowed_by_file`
/// and a commentable `+` line in that file. Used for a one-shot re-anchor bounce while steps
/// remain.
pub(crate) fn validate_batch_anchors(
    suggestions: &[Suggestion],
    allowed_by_file: &HashMap<String, &CommentableRange>,
) -> Result<(), String> {
    for s in suggestions {
        let file = normalize_path(&s.file);
        let Some(allowed) = allowed_by_file.get(&file) else {
            let mut valid_files: Vec<&str> = allowed_by_file.keys().map(String::as_str).collect();
            valid_files.sort_unstable();
            let valid_list = if valid_files.is_empty() {
                "(none)".to_string()
            } else {
                valid_files.join(", ")
            };
            return Err(format!(
                "file `{}` is not in this batch (valid files: {valid_list})",
                s.file
            ));
        };
        if !allowed.lines.contains(&s.line) {
            let mut nearest: Vec<u32> = allowed.lines.iter().copied().collect();
            nearest.sort_by_key(|&candidate| (candidate.abs_diff(s.line), candidate));
            nearest.truncate(5);
            nearest.sort_unstable();
            if nearest.is_empty() {
                return Err(format!(
                    "line {} in `{file}` is not a commentable `+` line (no commentable `+` lines in this file)",
                    s.line
                ));
            }
            let line_list = nearest
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            return Err(format!(
                "line {} in `{file}` is not a commentable `+` line (nearest valid `+` lines: {line_list})",
                s.line
            ));
        }
    }
    Ok(())
}

/// Keeps the findings of one Finder batch that can be posted: the normalized file is a key of
/// `allowed_by_file` (normalized paths) and `line` is in that file's commentable lines.
/// `start_line` survives only when `start_line..=line` lies in one diff hunk; when a requested
/// range is dropped, the `suggestion` goes too, because a multi-line replacement anchored to
/// `line` alone duplicates code on "Apply suggestion". Exact duplicates are then merged (see
/// `dedup_exact`). `file` in the result is the normalized path. `T` is a finding or a value
/// carrying one (a Verifier candidate keeps its other fields).
pub(crate) fn keep_valid_batch<T: AsRef<Suggestion> + AsMut<Suggestion>>(
    items: Vec<T>,
    allowed_by_file: &HashMap<String, &CommentableRange>,
) -> Vec<T> {
    let valid = items.into_iter().filter_map(|mut item| {
        let s = item.as_mut();
        let file = normalize_path(&s.file);
        let allowed = allowed_by_file.get(&file)?;
        if !allowed.lines.contains(&s.line) {
            return None;
        }
        match s.start_line {
            Some(start) if start == s.line => s.start_line = None,
            Some(start) if !allowed.spans_one_hunk(start, s.line) => {
                s.start_line = None;
                s.suggestion = None;
            }
            _ => {}
        }
        s.file = file;
        Some(item)
    });
    dedup_exact(valid, AsRef::as_ref)
}

/// Drops exact `(file, line, title)` duplicates of `items` (the body stands in for an empty
/// title, so two untitled lean findings on one line merge only when they say the same thing),
/// keeping the first position. When
/// the kept entry has no `start_line` and a later duplicate has one, the later entry takes that
/// position: its suggestion was written for the span it replaces.
pub(crate) fn dedup_exact<T>(
    items: impl IntoIterator<Item = T>,
    finding: impl Fn(&T) -> &Suggestion,
) -> Vec<T> {
    let mut kept: Vec<T> = Vec::new();
    let mut positions: HashMap<(String, u32, String), usize> = HashMap::new();
    for item in items {
        let s = finding(&item);
        let text = if s.title.is_empty() {
            &s.body
        } else {
            &s.title
        };
        let key = (s.file.clone(), s.line, text.clone());
        let has_range = s.start_line.is_some();
        match positions.get(&key).and_then(|&at| kept.get_mut(at)) {
            Some(slot) => {
                if has_range && finding(slot).start_line.is_none() {
                    *slot = item;
                }
            }
            None => {
                positions.insert(key, kept.len());
                kept.push(item);
            }
        }
    }
    kept
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sugg(file: &str, line: u32, title: &str) -> Suggestion {
        Suggestion {
            file: file.into(),
            start_line: None,
            line,
            severity: Some(Severity::Warning),
            category: None,
            title: title.into(),
            body: "body".into(),
            suggestion: None,
        }
    }

    fn sugg_span(file: &str, start_line: Option<u32>, line: u32, title: &str) -> Suggestion {
        Suggestion {
            start_line,
            ..sugg(file, line, title)
        }
    }

    #[test]
    fn parse_dismissed_skips_malformed_caps_cuts_and_redacts() {
        let ok = |file: &str| json!({"file": file, "suspicion": "s", "why_safe": "w"});
        let args = json!({"findings": [], "dismissed": [
            {"file": "a.go", "line": 12, "suspicion": "nil deref", "why_safe": "guarded at a.go:10"},
            ok("b.go"),
            {"file": "c.go", "line": -3, "suspicion": "s", "why_safe": "w"},
            {"file": "d.go", "suspicion": "s", "why_safe": "   "},
            {"file": "e.go", "suspicion": "s"},
            "not an object",
            {
                "file": "f.go",
                "suspicion": "é".repeat(400),
                "why_safe": "token ghp_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa leaked"
            },
        ]});
        let got = parse_dismissed(&args);
        assert_eq!(got.len(), 4);
        assert_eq!(
            got[0],
            Dismissal {
                file: "a.go".into(),
                line: Some(12),
                suspicion: "nil deref".into(),
                why_safe: "guarded at a.go:10".into(),
            }
        );
        assert_eq!(got[1].line, None);
        assert_eq!((got[2].file.as_str(), got[2].line), ("c.go", None));
        assert_eq!(got[3].suspicion.chars().count(), DISMISSED_TEXT_CHARS);
        assert!(got[3].why_safe.contains("[REDACTED]"));
        assert!(!got[3].why_safe.contains("ghp_"));

        for absent in [
            json!({"findings": []}),
            json!({"findings": [], "dismissed": {"file": "x"}}),
            json!({"findings": [], "dismissed": "x"}),
        ] {
            assert!(parse_dismissed(&absent).is_empty());
        }
        // A token straddling the cut, and an over-long `file` holding one, leak no part of it.
        let token = "ghp_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let straddling = format!("{} {token}", "x".repeat(DISMISSED_TEXT_CHARS - 10));
        let long_file = format!("{token}/{}", "d/".repeat(400));
        let cut = parse_dismissed(&json!({"dismissed": [
            {"file": long_file, "suspicion": straddling, "why_safe": "w"},
        ]}));
        assert_eq!(cut.len(), 1);
        for text in [&cut[0].file, &cut[0].suspicion] {
            assert!(text.chars().count() <= DISMISSED_TEXT_CHARS);
            assert!(
                !text.contains("ghp_") && !text.contains("aaaaaaaa"),
                "{text}"
            );
        }

        let many: Vec<Value> = (0..40).map(|i| ok(&format!("f{i}.go"))).collect();
        assert_eq!(
            parse_dismissed(&json!({"dismissed": many})).len(),
            MAX_DISMISSED
        );
    }

    #[test]
    fn parses_suggestions_formats_and_errors() {
        let fenced = "```json\n[{\"file\":\"src/a.rs\",\"line\":3,\"severity\":\"critical\",\
                      \"title\":\"T\",\"body\":\"B\",\"suggestion\":\"let x = 1;\"}]\n```";
        assert_eq!(
            parse_suggestions(fenced).unwrap(),
            vec![Suggestion {
                file: "src/a.rs".into(),
                start_line: None,
                line: 3,
                severity: Some(Severity::Critical),
                category: None,
                title: "T".into(),
                body: "B".into(),
                suggestion: Some("let x = 1;".into()),
            }]
        );

        let prose = "Sure! Here is the review [see below]:\n\
                     [{\"file\":\"a.rs\",\"line\":\"7\",\"severity\":\"warning\",\"title\":\"T\",\"body\":\"B\"}]\n\
                     Let me know if you need more.";
        let got = parse_suggestions(prose).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].line, 7);
        assert_eq!(got[0].severity, Some(Severity::Warning));
        assert_eq!(got[0].suggestion, None);

        // Empty brackets in prose do not mask a real finding array; empty arrays still parse.
        let with_brackets = "Found 1 issue in `main(String[] args)`:\n\
                             [{\"file\":\"a.rs\",\"line\":2,\"title\":\"T\",\"body\":\"B\"}]";
        assert_eq!(parse_suggestions(with_brackets).unwrap().len(), 1);
        for empty in ["- [ ] todo\n[]", "[]", "```json\n[ ]\n```"] {
            assert_eq!(parse_suggestions(empty).unwrap(), vec![], "{empty}");
        }
        for bad in ["I found no issues.", "[not json", ""] {
            assert_eq!(parse_suggestions(bad), Err(ParseError), "{bad}");
        }
    }

    #[test]
    fn skips_incomplete_items_leaves_unknown_severity_unset_and_sanitizes_start_line() {
        let raw = r#"[{"file":"a.rs","line":1,"severity":"blocker","title":"T","body":"B"},
                      {"file":"a.rs","line":2,"title":"T","body":"B"},
                      {"file":"a.rs","line":3,"severity":"CRITICAL","title":"T","body":"B"}]"#;
        let severities: Vec<_> = parse_suggestions(raw)
            .unwrap()
            .into_iter()
            .map(|s| s.severity)
            .collect();
        assert_eq!(severities, vec![None, None, Some(Severity::Critical)]);

        let incomplete = r#"[{"line":1,"title":"no file","body":"B"},
                             {"file":"a.rs","title":"no line","body":"B"},
                             {"file":"a.rs","line":-1,"title":"bad line","body":"B"},
                             {"file":"a.rs","line":"x","title":"bad line","body":"B"},
                             {"file":"a.rs","line":1,"title":"no body"},
                             {"file":"a.rs","line":9,"title":"ok","body":"B"}]"#;
        let got = parse_suggestions(incomplete).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].title, "ok");

        let ranges = r#"[
            {"file":"a.rs","start_line":2,"line":5,"title":"num","body":"B","suggestion":"x"},
            {"file":"a.rs","start_line":"3","line":5,"title":"str","body":"B","suggestion":"x"},
            {"file":"a.rs","start_line":5,"line":5,"title":"equal","body":"B","suggestion":"x"},
            {"file":"a.rs","start_line":null,"line":5,"title":"null","body":"B","suggestion":"x"},
            {"file":"a.rs","start_line":7,"line":5,"title":"inverted","body":"B","suggestion":"x"},
            {"file":"a.rs","start_line":0,"line":5,"title":"zero","body":"B","suggestion":"x"},
            {"file":"a.rs","start_line":"bad","line":5,"title":"bad_str","body":"B","suggestion":"x"}
        ]"#;
        let parsed: Vec<(String, Option<u32>, bool)> = parse_suggestions(ranges)
            .unwrap()
            .into_iter()
            .map(|s| (s.title, s.start_line, s.suggestion.is_some()))
            .collect();
        assert_eq!(
            parsed,
            [
                ("num".into(), Some(2), true),
                ("str".into(), Some(3), true),
                ("equal".into(), None, true),
                ("null".into(), None, true),
                ("inverted".into(), None, false),
                ("zero".into(), None, false),
                ("bad_str".into(), None, false),
            ]
        );
    }

    #[test]
    fn parse_submit_findings_validates_and_sanitizes() {
        let args = json!({"findings": [
            {"file": "src/a.rs", "start_line": 2, "line": 3, "severity": "critical", "title": "T",
             "body": "B", "suggestion": "let x = 1;", "category": "style"},
            {"file": "src/a.rs", "title": "no line", "body": "B"},
            "not an object",
            {"file": "src/b.rs", "start_line": "bad", "line": "4", "title": "T2", "body": "B2", "suggestion": "y"}
        ]});
        let got = parse_submit_findings(&args).unwrap();
        assert_eq!(
            got,
            vec![
                Suggestion {
                    file: "src/a.rs".into(),
                    start_line: Some(2),
                    line: 3,
                    severity: Some(Severity::Critical),
                    category: None,
                    title: "T".into(),
                    body: "B".into(),
                    suggestion: Some("let x = 1;".into()),
                },
                Suggestion {
                    file: "src/b.rs".into(),
                    start_line: None,
                    line: 4,
                    severity: None,
                    category: None,
                    title: "T2".into(),
                    body: "B2".into(),
                    suggestion: None,
                },
            ]
        );
        assert_eq!(
            parse_submit_findings(&json!({"findings": []})).unwrap(),
            vec![]
        );
    }

    #[test]
    fn submit_findings_tool_def_uses_portable_schema() {
        let def = submit_findings_tool_def(true);
        assert_eq!(def.name, "submit_findings");
        let params = &def.parameters;
        assert_eq!(params["required"], json!(["review_log", "findings"]));
        let keys: Vec<&str> = params["properties"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, ["review_log", "findings", "dismissed"]);
        assert_eq!(params["properties"]["review_log"]["type"], json!("string"));
        // The schema never asks for the model's reasoning (Anthropic `reasoning_extraction`).
        assert!(!def.description.to_lowercase().contains("reason"));
        assert!(!params.to_string().to_lowercase().contains("reason"));
        let item = &params["properties"]["findings"]["items"];
        assert_eq!(
            item["required"],
            json!(["file", "line", "category", "body"])
        );
        let fields: Vec<&str> = item["properties"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(fields, ["file", "start_line", "line", "category", "body"]);
        assert_eq!(item["properties"]["start_line"]["type"], json!("integer"));
        assert_eq!(
            item["properties"]["category"]["enum"],
            json!(["bug", "security", "performance", "repo_instruction"])
        );
        // A specialist's findings default to its own category.
        let specialist = submit_findings_tool_def(false).parameters;
        assert_eq!(
            specialist["properties"]["findings"]["items"]["required"],
            json!(["file", "line", "body"])
        );
        // `dismissed` is optional: `required` above does not list it.
        let dismissed = &params["properties"]["dismissed"];
        assert_eq!(dismissed["type"], json!("array"));
        assert_eq!(
            dismissed["items"]["required"],
            json!(["file", "suspicion", "why_safe"])
        );
        assert_eq!(
            dismissed["items"]["properties"]["line"]["type"],
            json!("integer")
        );
        let text = params.to_string();
        for banned in ["additionalProperties", "$schema", "oneOf"] {
            assert!(!text.contains(banned), "schema uses {banned}");
        }
    }

    #[test]
    fn parses_lean_finder_findings_and_review_log() {
        let lean = json!({
            "review_log": "GetCache: read both callers; the nil error path is reported.",
            "findings": [{"file": "src/a.rs", "line": 4, "category": "bug", "body": "why"}]
        });
        let expected = vec![Suggestion {
            file: "src/a.rs".into(),
            start_line: None,
            line: 4,
            severity: None,
            category: Some(FindingCategory::Bug),
            title: String::new(),
            body: "why".into(),
            suggestion: None,
        }];
        assert_eq!(parse_submit_findings(&lean).unwrap(), expected);
        assert_eq!(
            parse_review_log(&lean).as_deref(),
            Some("GetCache: read both callers; the nil error path is reported.")
        );
        // The field's earlier name is still read.
        assert_eq!(
            parse_review_log(&json!({"reasoning": " read a.rs "})).as_deref(),
            Some("read a.rs")
        );

        // A missing or blank `review_log` never refuses the submission.
        let mut without = lean;
        without.as_object_mut().unwrap().remove("review_log");
        assert_eq!(parse_submit_findings(&without).unwrap(), expected);
        assert_eq!(parse_review_log(&without), None);
        assert_eq!(parse_review_log(&json!({"review_log": "  "})), None);
    }

    #[test]
    fn parses_stringified_findings_and_dismissed_arrays() {
        let args = json!({
            "review_log": "r",
            "findings": "[{\"file\": \"src/a.rs\", \"line\": 4, \"category\": \"bug\", \"body\": \"why\"}]",
            "dismissed": "[{\"file\": \"src/a.rs\", \"suspicion\": \"s\", \"why_safe\": \"w\"}]"
        });
        let got = parse_submit_findings(&args).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!((got[0].line, got[0].body.as_str()), (4, "why"));
        assert_eq!(parse_dismissed(&args).len(), 1);

        for bad in [
            json!({}),
            json!({"findings": "not json"}),
            json!({"findings": "{\"file\": \"src/a.rs\"}"}),
            json!({"findings": {"file": "src/a.rs"}}),
        ] {
            let err = parse_submit_findings(&bad).unwrap_err();
            assert!(err.contains("findings"), "{err}");
        }
    }

    #[test]
    #[expect(
        clippy::single_range_in_vec_init,
        reason = "hunk list contains a single inclusive line range"
    )]
    fn keep_valid_batch_filters_files_lines_hunks_and_dedups() {
        let ra = CommentableRange {
            lines: BTreeSet::from([2, 3]),
            hunks: vec![1..=4],
        };
        let rb = CommentableRange {
            lines: BTreeSet::from([5]),
            hunks: vec![4..=6],
        };
        let allowed = HashMap::from([("src/a.rs".to_string(), &ra), ("src/b.rs".to_string(), &rb)]);
        let got = keep_valid_batch(
            vec![
                sugg("/src/a.rs", 2, "T"),
                sugg("src/a.rs", 2, "T"),
                sugg("src/a.rs", 4, "context line"),
                sugg("src/b.rs", 5, "B"),
                sugg("src/b.rs", 2, "line of another file"),
                sugg("src/c.rs", 5, "file outside the batch"),
                sugg("src/a.rs", 2, "other title"),
            ],
            &allowed,
        );
        assert_eq!(
            got,
            vec![
                sugg("src/a.rs", 2, "T"),
                sugg("src/b.rs", 5, "B"),
                sugg("src/a.rs", 2, "other title"),
            ]
        );

        let patch = "@@ -1,2 +1,3 @@\n ctx\n+a\n+b\n@@ -10,2 +11,3 @@\n x\n+y\n z\n";
        let range = CommentableRange::from_patch(patch);
        let allowed = HashMap::from([("src/a.rs".to_string(), &range)]);
        let with_fence = |start_line, line, title| Suggestion {
            suggestion: Some("one\ntwo".into()),
            ..sugg_span("src/a.rs", start_line, line, title)
        };
        let batch = keep_valid_batch(
            vec![
                with_fence(Some(1), 3, "context start in same hunk"),
                with_fence(Some(2), 12, "cross hunk"),
                with_fence(Some(9), 12, "before hunk"),
                with_fence(None, 12, "single line"),
            ],
            &allowed,
        );
        let kept: Vec<_> = batch
            .iter()
            .map(|s| (s.start_line, s.suggestion.is_some()))
            .collect();
        assert_eq!(
            kept,
            [(Some(1), true), (None, false), (None, false), (None, true)]
        );
    }

    #[test]
    fn exact_duplicates_prefer_a_valid_range_and_keep_the_first_position() {
        let patch = "@@ -1,1 +1,4 @@\n a\n+b\n+c\n+d\n";
        let range = CommentableRange::from_patch(patch);
        let allowed = HashMap::from([("src/a.rs".to_string(), &range)]);
        let got = keep_valid_batch(
            vec![
                sugg("src/a.rs", 3, "T"),
                sugg("src/a.rs", 4, "other"),
                sugg_span("src/a.rs", Some(2), 3, "T"),
                sugg_span("src/a.rs", Some(3), 3, "T"),
            ],
            &allowed,
        );
        assert_eq!(
            got,
            vec![
                sugg_span("src/a.rs", Some(2), 3, "T"),
                sugg("src/a.rs", 4, "other"),
            ]
        );
    }

    #[test]
    fn validate_batch_anchors_rejects_bad_file_or_line_and_suggests_nearest() {
        let range = CommentableRange {
            lines: BTreeSet::from([10, 12, 20, 21, 22, 30]),
            hunks: Vec::new(),
        };
        let allowed = HashMap::from([("src/a.rs".to_string(), &range)]);

        let bad_file = vec![sugg("src/missing.rs", 10, "Wrong file")];
        let err = validate_batch_anchors(&bad_file, &allowed).unwrap_err();
        assert!(err.contains("src/missing.rs"), "{err}");
        assert!(err.contains("src/a.rs"), "{err}");

        let bad_line = vec![sugg("src/a.rs", 14, "Off-line")];
        let err = validate_batch_anchors(&bad_line, &allowed).unwrap_err();
        assert!(err.contains("14"), "{err}");
        assert!(err.contains("src/a.rs"), "{err}");
        assert!(err.contains("10, 12, 20, 21, 22"), "{err}");
        assert!(!err.contains("30"), "{err}");

        // Tie-breaking: distance 7 from line 17 to both 10 and 24 picks smaller 10.
        let tie_range = CommentableRange {
            lines: BTreeSet::from([10, 12, 16, 20, 22, 24]),
            hunks: Vec::new(),
        };
        let tie_allowed = HashMap::from([("src/a.rs".to_string(), &tie_range)]);
        let tie_err =
            validate_batch_anchors(&[sugg("src/a.rs", 17, "Tie")], &tie_allowed).unwrap_err();
        assert!(tie_err.contains("10, 12, 16, 20, 22"), "{tie_err}");
        assert!(!tie_err.contains("24"), "{tie_err}");

        let normalized_ok = vec![sugg("/src/a.rs", 12, "Valid")];
        assert_eq!(validate_batch_anchors(&normalized_ok, &allowed), Ok(()));
    }

    #[test]
    fn first_json_value_skips_non_matching_and_handles_prose_and_fences() {
        let raw = "Note: `if { x }` and {\"other\": 1} then\n```json\n{\"keep\": true, \"n\": 2}\n```\ntrailing";
        let got = first_json_value(raw, |v| v.get("keep").and_then(Value::as_bool));
        assert_eq!(got, Some(true));

        let none = first_json_value("no objects {\"a\": 1}", |v| v.get("b").cloned());
        assert_eq!(none, None);
    }

    #[test]
    fn parse_bool_field_accepts_bool_and_trimmed_ascii_strings() {
        assert_eq!(parse_bool_field(&json!(true)), Some(true));
        assert_eq!(parse_bool_field(&json!(false)), Some(false));
        assert_eq!(parse_bool_field(&json!("  true \n")), Some(true));
        assert_eq!(parse_bool_field(&json!("\tFALSE ")), Some(false));
        for invalid in [json!("yes"), json!("1"), json!(""), json!(1), json!(null)] {
            assert_eq!(parse_bool_field(&invalid), None, "{invalid}");
        }
    }
}
