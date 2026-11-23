//! Loads repository instruction files (`AGENTS.md` and friends) from a review workspace.
//!
//! Values that look like secrets are masked ([`redact_secrets`]); the prompt code wraps the
//! text and neutralizes tags.

use std::sync::Arc;

use crate::redact::redact_secrets;
use crate::tools::read_text;
use crate::workspace::RepoWorkspace;

/// Instruction files read first, in this order.
const KNOWN_FILES: &[&str] = &["AGENTS.md", "CLAUDE.md", ".github/copilot-instructions.md"];
/// Directory whose `*.mdc` files follow, sorted by name.
const CURSOR_RULES_DIR: &str = ".cursor/rules";
/// Cap on the whole result, marker included.
const MAX_CHARS: usize = 8_000;
const TRUNCATED: &str = "\n[truncated]";

/// Each instruction file as `--- <path> ---` followed by its text, blocks separated by a blank
/// line and capped at 8,000 characters. Missing, unreadable, binary and blank files are
/// skipped; `None` when nothing is left.
pub(crate) async fn load_repo_instructions(workspace: Arc<RepoWorkspace>) -> Option<String> {
    match tokio::task::spawn_blocking(move || load_sync(&workspace)).await {
        Ok(text) => text,
        Err(err) => {
            tracing::warn!(error = %err, "loading repository instructions failed");
            None
        }
    }
}

fn load_sync(ws: &RepoWorkspace) -> Option<String> {
    let mut out = String::new();
    let mut chars = 0;
    for rel in candidate_paths(ws) {
        let Ok(path) = ws.resolve_existing_file(&rel) else {
            continue;
        };
        let Ok(text) = read_text(path.abs()) else {
            continue;
        };
        let text = text.replace("\r\n", "\n");
        // Each whole file is redacted before the cap, so a cut never leaves part of a secret.
        let text = redact_secrets(&text);
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        let separator = if out.is_empty() { "" } else { "\n\n" };
        let block = format!("{separator}--- {} ---\n{text}", path.rel());
        let block_chars = block.chars().count();
        if chars + block_chars > MAX_CHARS {
            let room = MAX_CHARS.saturating_sub(chars + TRUNCATED.len());
            out.extend(block.chars().take(room));
            out.push_str(TRUNCATED);
            break;
        }
        out.push_str(&block);
        chars += block_chars;
    }
    (!out.is_empty()).then_some(out)
}

/// The known files, then `.cursor/rules/*.mdc` sorted lexicographically.
fn candidate_paths(ws: &RepoWorkspace) -> Vec<String> {
    let mut paths: Vec<String> = KNOWN_FILES.iter().map(|p| (*p).to_owned()).collect();
    let Ok(dir) = ws.resolve_dir(CURSOR_RULES_DIR) else {
        return paths;
    };
    let Ok(entries) = std::fs::read_dir(dir.abs()) else {
        return paths;
    };
    let mut rules: Vec<String> = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|t| t.is_file()))
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| name.ends_with(".mdc"))
        .map(|name| format!("{CURSOR_RULES_DIR}/{name}"))
        .collect();
    rules.sort();
    paths.extend(rules);
    paths
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace(files: &[(&str, &[u8])]) -> Arc<RepoWorkspace> {
        Arc::new(RepoWorkspace::from_files(files))
    }

    #[tokio::test]
    async fn loads_known_files_in_order_with_sorted_cursor_rules() {
        let ws = workspace(&[
            (".cursor/rules/b.mdc", b"b rule\n"),
            (".cursor/rules/a.mdc", b"a rule\r\n"),
            (".cursor/rules/notes.txt", b"not a rule"),
            (".cursor/rules/bin.mdc", b"\0binary"),
            (".github/copilot-instructions.md", b"copilot"),
            ("CLAUDE.md", b"claude\n"),
            ("AGENTS.md", b"agents\n"),
            ("README.md", b"readme"),
        ]);
        assert_eq!(
            load_repo_instructions(ws).await.as_deref(),
            Some(
                "--- AGENTS.md ---\nagents\n\n--- CLAUDE.md ---\nclaude\n\n\
                 --- .github/copilot-instructions.md ---\ncopilot\n\n\
                 --- .cursor/rules/a.mdc ---\na rule\n\n--- .cursor/rules/b.mdc ---\nb rule"
            )
        );
    }

    #[tokio::test]
    async fn skips_missing_and_blank_files_and_returns_none_without_any() {
        let ws = workspace(&[("CLAUDE.md", b"only claude")]);
        assert_eq!(
            load_repo_instructions(ws).await.as_deref(),
            Some("--- CLAUDE.md ---\nonly claude")
        );
        let ws = workspace(&[("README.md", b"readme"), ("AGENTS.md", b"  \n")]);
        assert_eq!(load_repo_instructions(ws).await, None);
    }

    #[tokio::test]
    async fn caps_total_at_8000_chars() {
        let long = "\u{e9}".repeat(10_000);
        let ws = workspace(&[("AGENTS.md", long.as_bytes()), ("CLAUDE.md", b"claude")]);
        let text = load_repo_instructions(ws).await.unwrap();
        assert!(text.chars().count() <= 8_000);
        assert!(text.starts_with("--- AGENTS.md ---\n\u{e9}\u{e9}"));
        assert!(text.ends_with("\n[truncated]"));
        assert!(!text.contains("CLAUDE.md"));
    }

    #[tokio::test]
    async fn repo_instructions_are_redacted() {
        let ws = workspace(&[
            (
                "AGENTS.md",
                b"Deploy with:\nexport DEPLOY_API_KEY=abcdefgh12345\nThe CI token is ghp_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.\n",
            ),
            ("CLAUDE.md", b"Use tracing."),
        ]);
        assert_eq!(
            load_repo_instructions(ws).await.as_deref(),
            Some(
                "--- AGENTS.md ---\nDeploy with:\nexport DEPLOY_API_KEY=[REDACTED]\n\
                 The CI token is [REDACTED].\n\n--- CLAUDE.md ---\nUse tracing."
            )
        );
    }
}
