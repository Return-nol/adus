//! Parsing dan validasi jawaban LLM menjadi daftar temuan review.

use std::collections::{BTreeSet, HashMap, HashSet};

use adus_core::types::{Severity, normalize_path};
use adus_git::FileDiff;
use adus_llm::ToolDef;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};
use thiserror::Error;

/// Satu temuan dari LLM untuk satu baris di file baru.
#[derive(Debug, Clone, PartialEq)]
pub struct Suggestion {
    pub file: String,
    pub line: u32,
    pub severity: Severity,
    pub title: String,
    pub body: String,
    /// Kode pengganti untuk baris itu (opsional).
    pub suggestion: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("no JSON array found in the model output")]
pub struct ParseError;

/// Ambil array JSON pertama dari jawaban model (boleh dibungkus ```json atau diapit prosa).
/// Objek yang kurang field wajib (`file`, `line`, `title`, `body`) dilewati, bukan menggagalkan parse.
pub fn parse_suggestions(raw: &str) -> Result<Vec<Suggestion>, ParseError> {
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

fn to_suggestion(item: &Map<String, Value>) -> Option<Suggestion> {
    let text = |key: &str| item.get(key)?.as_str().map(str::to_string);
    let line = match item.get("line")? {
        Value::Number(n) => n.as_u64()?,
        Value::String(s) => s.trim().parse().ok()?,
        _ => return None,
    };
    let severity = match text("severity")
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "critical" => Severity::Critical,
        "warning" => Severity::Warning,
        _ => Severity::Info,
    };
    let file = text("file").filter(|f| !f.trim().is_empty())?;
    Some(Suggestion {
        file,
        line: u32::try_from(line).ok()?,
        severity,
        title: text("title")?,
        body: text("body")?,
        suggestion: text("suggestion"),
    })
}

/// Buang temuan untuk file lain atau baris yang tidak bisa dikomentari, lalu dedup `(file, line, title)`.
/// `file` di hasil diganti dengan path file yang sudah dinormalisasi.
pub fn keep_valid(
    suggestions: Vec<Suggestion>,
    file: &FileDiff,
    allowed_lines: &BTreeSet<u32>,
) -> Vec<Suggestion> {
    let path = normalize_path(&file.path);
    let mut seen = HashSet::new();
    suggestions
        .into_iter()
        .filter(|s| normalize_path(&s.file) == path && allowed_lines.contains(&s.line))
        .map(|s| Suggestion {
            file: path.clone(),
            ..s
        })
        .filter(|s| seen.insert((s.file.clone(), s.line, s.title.clone())))
        .collect()
}

/// The Finder's terminal tool. Uses only `type`, `properties`, `required`, `description`,
/// `items` and `enum`, the JSON-schema subset every provider (Gemini included) accepts.
pub fn submit_findings_tool_def() -> ToolDef {
    ToolDef {
        name: "submit_findings".into(),
        description: "Submit your final review findings and end the review. Call it exactly \
            once, with every finding you confirmed, or with an empty list when nothing is worth \
            reporting."
            .into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "findings": {
                    "type": "array",
                    "description": "The confirmed findings; empty when there is nothing to report.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "file": {
                                "type": "string",
                                "description": "Path of the changed file exactly as shown in <diffs>."
                            },
                            "line": {
                                "type": "integer",
                                "description": "New-file line number of a \"+\" line in that file's diff."
                            },
                            "severity": {
                                "type": "string",
                                "enum": ["info", "warning", "critical"],
                                "description": "critical: will break, lose data or open a security hole; warning: likely bug or real performance or maintainability problem; info: worth knowing, low risk."
                            },
                            "title": {
                                "type": "string",
                                "description": "One-line summary of the problem."
                            },
                            "body": {
                                "type": "string",
                                "description": "Why it is a problem and how to fix it."
                            },
                            "suggestion": {
                                "type": "string",
                                "description": "Optional exact replacement code for that line, keeping its indentation; may span several lines. Leave it out when unsure."
                            }
                        },
                        "required": ["file", "line", "severity", "title", "body"]
                    }
                }
            },
            "required": ["findings"]
        }),
    }
}

/// Arguments of `submit_findings`. Elements stay untyped so one malformed finding is skipped
/// instead of rejecting the whole submission.
#[derive(Deserialize)]
struct SubmitFindingsArgs {
    findings: Vec<Value>,
}

/// Reads the `submit_findings` arguments. Elements that are not objects or lack a required
/// field are skipped, as in [`parse_suggestions`]. The error text is shown to the model.
pub fn parse_submit_findings(args: &Value) -> Result<Vec<Suggestion>, String> {
    let parsed = SubmitFindingsArgs::deserialize(args).map_err(|err| {
        format!("{err}; expected an object with a `findings` array of finding objects")
    })?;
    Ok(parsed
        .findings
        .iter()
        .filter_map(Value::as_object)
        .filter_map(to_suggestion)
        .collect())
}

/// [`keep_valid`] for a batch of files: keeps findings whose normalized file is a key of
/// `allowed_by_file` (normalized paths) and whose line is in that file's set, then dedups
/// `(file, line, title)`. `file` in the result is the normalized path.
pub fn keep_valid_batch(
    suggestions: Vec<Suggestion>,
    allowed_by_file: &HashMap<String, BTreeSet<u32>>,
) -> Vec<Suggestion> {
    let mut seen = HashSet::new();
    suggestions
        .into_iter()
        .filter_map(|s| {
            let file = normalize_path(&s.file);
            let allowed = allowed_by_file.get(&file)?;
            allowed
                .contains(&s.line)
                .then_some(Suggestion { file, ..s })
        })
        .filter(|s| seen.insert((s.file.clone(), s.line, s.title.clone())))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use adus_git::FileStatus;

    fn file(path: &str) -> FileDiff {
        FileDiff {
            path: path.into(),
            old_path: None,
            status: FileStatus::Modified,
            patch: Some("@@ -1,1 +1,2 @@\n a\n+b\n".into()),
        }
    }

    fn sugg(file: &str, line: u32, title: &str) -> Suggestion {
        Suggestion {
            file: file.into(),
            line,
            severity: Severity::Warning,
            title: title.into(),
            body: "body".into(),
            suggestion: None,
        }
    }

    #[test]
    fn parses_fenced_json() {
        let raw = "```json\n[{\"file\":\"src/a.rs\",\"line\":3,\"severity\":\"critical\",\
                   \"title\":\"T\",\"body\":\"B\",\"suggestion\":\"let x = 1;\"}]\n```";
        let got = parse_suggestions(raw).unwrap();
        assert_eq!(
            got,
            vec![Suggestion {
                file: "src/a.rs".into(),
                line: 3,
                severity: Severity::Critical,
                title: "T".into(),
                body: "B".into(),
                suggestion: Some("let x = 1;".into()),
            }]
        );
    }

    #[test]
    fn parses_with_leading_prose() {
        let raw = "Sure! Here is the review [see below]:\n\
                   [{\"file\":\"a.rs\",\"line\":\"7\",\"severity\":\"warning\",\"title\":\"T\",\"body\":\"B\"}]\n\
                   Let me know if you need more.";
        let got = parse_suggestions(raw).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].line, 7, "numeric string line is accepted");
        assert_eq!(got[0].severity, Severity::Warning);
        assert_eq!(got[0].suggestion, None);
    }

    #[test]
    fn empty_bracket_in_prose_does_not_hide_findings() {
        let raw = "Found 1 issue in `main(String[] args)`:
                   [{\"file\":\"a.rs\",\"line\":2,\"title\":\"T\",\"body\":\"B\"}]";
        assert_eq!(parse_suggestions(raw).unwrap().len(), 1);
        assert_eq!(
            parse_suggestions(
                "- [ ] todo
[]"
            )
            .unwrap(),
            vec![]
        );
    }

    #[test]
    fn empty_array_ok() {
        assert_eq!(parse_suggestions("[]").unwrap(), vec![]);
        assert_eq!(parse_suggestions("```json\n[ ]\n```").unwrap(), vec![]);
    }

    #[test]
    fn garbage_is_error() {
        assert_eq!(parse_suggestions("I found no issues."), Err(ParseError));
        assert_eq!(parse_suggestions("[not json"), Err(ParseError));
        assert_eq!(parse_suggestions(""), Err(ParseError));
    }

    #[test]
    fn unknown_severity_defaults_info() {
        let raw = r#"[{"file":"a.rs","line":1,"severity":"blocker","title":"T","body":"B"},
                      {"file":"a.rs","line":2,"title":"T","body":"B"},
                      {"file":"a.rs","line":3,"severity":"CRITICAL","title":"T","body":"B"}]"#;
        let got = parse_suggestions(raw).unwrap();
        let severities: Vec<_> = got.iter().map(|s| s.severity).collect();
        assert_eq!(
            severities,
            vec![Severity::Info, Severity::Info, Severity::Critical]
        );
    }

    #[test]
    fn skips_objects_missing_required_fields() {
        let raw = r#"[{"line":1,"title":"no file","body":"B"},
                      {"file":"a.rs","title":"no line","body":"B"},
                      {"file":"a.rs","line":-1,"title":"bad line","body":"B"},
                      {"file":"a.rs","line":"x","title":"bad line","body":"B"},
                      {"file":"a.rs","line":1,"body":"no title"},
                      {"file":"a.rs","line":1,"title":"no body"},
                      {"file":"a.rs","line":9,"title":"ok","body":"B"}]"#;
        let got = parse_suggestions(raw).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].title, "ok");
    }

    #[test]
    fn drops_lines_outside_diff() {
        let allowed = BTreeSet::from([2]);
        let got = keep_valid(
            vec![sugg("a.rs", 2, "in"), sugg("a.rs", 1, "context line")],
            &file("a.rs"),
            &allowed,
        );
        assert_eq!(got, vec![sugg("a.rs", 2, "in")]);
    }

    #[test]
    fn drops_other_files() {
        let allowed = BTreeSet::from([2]);
        let got = keep_valid(
            vec![
                sugg("/src/a.rs", 2, "azure style path"),
                sugg("src/b.rs", 2, "other file"),
            ],
            &file("src/a.rs"),
            &allowed,
        );
        assert_eq!(got, vec![sugg("src/a.rs", 2, "azure style path")]);
    }

    #[test]
    fn dedups_same_file_line_title() {
        let allowed = BTreeSet::from([2]);
        let got = keep_valid(
            vec![
                sugg("a.rs", 2, "T"),
                sugg("/a.rs", 2, "T"),
                sugg("a.rs", 2, "other"),
            ],
            &file("a.rs"),
            &allowed,
        );
        assert_eq!(got, vec![sugg("a.rs", 2, "T"), sugg("a.rs", 2, "other")]);
    }

    #[test]
    fn parse_submit_findings_extracts_valid_items_and_skips_incomplete() {
        let args = serde_json::json!({"findings": [
            {"file": "src/a.rs", "line": 3, "severity": "critical", "title": "T",
             "body": "B", "suggestion": "let x = 1;"},
            {"file": "src/a.rs", "title": "no line", "body": "B"},
            "not an object",
            {"file": "src/b.rs", "line": "4", "title": "T2", "body": "B2"}
        ]});
        let got = parse_submit_findings(&args).unwrap();
        assert_eq!(
            got,
            vec![
                Suggestion {
                    file: "src/a.rs".into(),
                    line: 3,
                    severity: Severity::Critical,
                    title: "T".into(),
                    body: "B".into(),
                    suggestion: Some("let x = 1;".into()),
                },
                Suggestion {
                    file: "src/b.rs".into(),
                    line: 4,
                    severity: Severity::Info,
                    title: "T2".into(),
                    body: "B2".into(),
                    suggestion: None,
                },
            ]
        );
        assert_eq!(
            parse_submit_findings(&serde_json::json!({"findings": []})).unwrap(),
            vec![]
        );
        let missing = parse_submit_findings(&serde_json::json!({})).unwrap_err();
        assert!(missing.contains("findings"), "{missing}");
        let wrong_type =
            parse_submit_findings(&serde_json::json!({"findings": "none"})).unwrap_err();
        assert!(wrong_type.contains("findings"), "{wrong_type}");
    }

    #[test]
    fn submit_findings_tool_def_uses_portable_schema() {
        let def = submit_findings_tool_def();
        assert_eq!(def.name, "submit_findings");
        let params = &def.parameters;
        assert_eq!(params["required"], serde_json::json!(["findings"]));
        let item = &params["properties"]["findings"]["items"];
        assert_eq!(
            item["required"],
            serde_json::json!(["file", "line", "severity", "title", "body"])
        );
        assert_eq!(
            item["properties"]["severity"]["enum"],
            serde_json::json!(["info", "warning", "critical"])
        );
        let text = params.to_string();
        for banned in ["additionalProperties", "$schema", "oneOf"] {
            assert!(!text.contains(banned), "schema uses {banned}");
        }
    }

    #[test]
    fn keep_valid_batch_filters_across_multiple_files_and_dedups() {
        let allowed = HashMap::from([
            ("src/a.rs".to_string(), BTreeSet::from([2, 3])),
            ("src/b.rs".to_string(), BTreeSet::from([5])),
        ]);
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
    }
}
