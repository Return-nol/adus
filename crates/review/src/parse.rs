//! Parsing dan validasi jawaban LLM menjadi daftar temuan review.

use std::collections::{BTreeSet, HashSet};

use adus_core::types::{Severity, normalize_path};
use adus_git::FileDiff;
use serde_json::{Map, Value};
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

/// Coba parse dari setiap `[` sampai ketemu array yang semua elemennya objek (atau kosong).
/// Teks setelah array diabaikan. Syarat "semua objek" mencegah `[1]` di prosa ikut terambil.
// ponytail: O(n²) di teks dengan banyak `[` yang bukan JSON; jawaban LLM kecil, jadi aman.
fn first_json_array(raw: &str) -> Option<Vec<Map<String, Value>>> {
    raw.match_indices('[').find_map(|(start, _)| {
        serde_json::Deserializer::from_str(&raw[start..])
            .into_iter::<Vec<Map<String, Value>>>()
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
}
