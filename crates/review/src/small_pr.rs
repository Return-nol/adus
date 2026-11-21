//! Small, low-risk PRs are reviewed from the diff only (no clone, no repository tools, one
//! Generalist pass). This file decides whether a PR qualifies; the engine acts on it.

use adus_core::types::{Severity, normalize_path};
use adus_db::rules::{Rule, rule_matches_path};
use adus_git::FileDiff;
use adus_git::diff::hunk_right_start;

/// A PR with more reviewed files than this is never small.
pub const SMALL_PR_MAX_FILES: usize = 3;

/// Path segments (directories and the file name) that contain one of these words are
/// security or access code. Conservative on purpose: `author.rs` matches `auth`.
const SENSITIVE_WORDS: [&str; 8] = [
    "auth",
    "security",
    "crypto",
    "permission",
    "secret",
    "token",
    "session",
    "password",
];

/// File names that define a shared module surface.
const CONTRACT_FILES: [&str; 4] = ["lib.rs", "mod.rs", "index.ts", "index.js"];

/// True when the PR is cheap and safe enough to review from the diff alone: `max_lines` is
/// above 0, there are one to `SMALL_PR_MAX_FILES` reviewed files, their changed lines (`+`
/// and `-` inside hunks) fit in `max_lines`, none is on a risky path (renames: old path too)
/// and none is covered by an enabled `critical` rule.
pub fn is_small_pr(targets: &[FileDiff], rules: &[Rule], max_lines: u32) -> bool {
    if max_lines == 0 || targets.is_empty() || targets.len() > SMALL_PR_MAX_FILES {
        return false;
    }
    let changed: usize = targets.iter().map(changed_lines).sum();
    if changed > max_lines as usize {
        return false;
    }
    let risky = targets.iter().any(|file| {
        is_risky_path(&file.path) || file.old_path.as_deref().is_some_and(is_risky_path)
    });
    let critical = rules.iter().any(|rule| {
        rule.enabled
            && rule.severity == Severity::Critical
            && targets
                .iter()
                .any(|file| rule_matches_path(rule, &file.path))
    });
    !risky && !critical
}

/// Number of `+` and `-` lines inside the hunks of one file's patch.
fn changed_lines(file: &FileDiff) -> usize {
    let Some(patch) = &file.patch else {
        return 0;
    };
    let mut in_hunk = false;
    let mut count = 0;
    for line in patch.lines() {
        if hunk_right_start(line).is_some() {
            in_hunk = true;
        } else if in_hunk && (line.starts_with('+') || line.starts_with('-')) {
            count += 1;
        }
    }
    count
}

/// Paths where a mistake costs more than the diff shows: security, data, shared contracts and
/// build/deploy files. Case-insensitive, on the normalized path.
fn is_risky_path(path: &str) -> bool {
    let path = normalize_path(path).to_lowercase();
    let segments: Vec<&str> = path.split('/').collect();
    let Some(name) = segments.last().copied() else {
        return false;
    };
    let dirs = &segments[..segments.len() - 1];
    let (stem, extension) = match name.rsplit_once('.') {
        Some((stem, extension)) => (stem, extension),
        None => (name, ""),
    };

    let sensitive = segments
        .iter()
        .any(|segment| SENSITIVE_WORDS.iter().any(|word| segment.contains(word)));
    let data = dirs.contains(&"migrations") || extension == "sql" || stem == "schema";
    let contract = matches!(extension, "proto" | "graphql")
        || name.starts_with("openapi.")
        || CONTRACT_FILES.contains(&name);
    let build = name.starts_with("dockerfile")
        || path.starts_with(".github/workflows/")
        || name == "cargo.toml"
        || name == "package.json";
    sensitive || data || contract || build
}

#[cfg(test)]
mod tests {
    use super::*;
    use adus_db::rules::KnowledgeScope;
    use adus_git::FileStatus;
    use chrono::Utc;
    use uuid::Uuid;

    fn target(path: &str, plus: usize, minus: usize) -> FileDiff {
        let mut patch = format!("@@ -1,{minus} +1,{plus} @@\n");
        patch.push_str(&"-old\n".repeat(minus));
        patch.push_str(&"+new\n".repeat(plus));
        FileDiff {
            path: path.into(),
            old_path: None,
            status: FileStatus::Modified,
            patch: Some(patch),
        }
    }

    fn rule(severity: Severity, glob: Option<&str>, enabled: bool) -> Rule {
        Rule {
            id: Uuid::nil(),
            scope: KnowledgeScope::Global,
            repository_id: None,
            title: "t".into(),
            body: "b".into(),
            path_glob: glob.map(str::to_string),
            severity,
            enabled,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn small_when_few_lines_and_files_outside_risky_paths() {
        let targets = [target("src/a.rs", 3, 2), target("web/b.svelte", 1, 0)];
        assert!(is_small_pr(&targets, &[], 50));
        // Exactly at the limit still counts as small.
        assert!(is_small_pr(&targets, &[], 6));
    }

    #[test]
    fn not_small_above_line_or_file_limit() {
        let targets = [target("src/a.rs", 3, 2)];
        assert!(!is_small_pr(&targets, &[], 4));
        let many: Vec<FileDiff> = (0..=SMALL_PR_MAX_FILES)
            .map(|i| target(&format!("src/f{i}.rs"), 1, 0))
            .collect();
        assert!(!is_small_pr(&many, &[], 1000));
        assert!(!is_small_pr(&[], &[], 1000));
    }

    #[test]
    fn zero_threshold_disables_small_pr() {
        assert!(!is_small_pr(&[target("src/a.rs", 1, 0)], &[], 0));
    }

    #[test]
    fn risky_paths_force_full_review() {
        let risky = [
            "src/auth/login.rs",
            "crates/core/src/crypto.rs",
            "src/Session.ts",
            "config/secrets.yml",
            "migrations/0009_x.sql",
            "db/seed.sql",
            "db/Schema.rb",
            "api/service.proto",
            "docs/openapi.yaml",
            "api/query.graphql",
            "crates/x/src/lib.rs",
            "src/foo/mod.rs",
            "web/src/index.ts",
            "Dockerfile",
            "docker/Dockerfile.prod",
            ".github/workflows/ci.yml",
            "crates/x/Cargo.toml",
            "web/package.json",
            "/src/auth.rs",
        ];
        for path in risky {
            assert!(!is_small_pr(&[target(path, 1, 0)], &[], 50), "{path}");
        }
        // A rename whose old path is risky.
        let mut renamed = target("src/handler.rs", 1, 0);
        renamed.old_path = Some("src/auth/handler.rs".into());
        assert!(!is_small_pr(&[renamed], &[], 50));
        // Control: an ordinary path is small.
        assert!(is_small_pr(&[target("src/handler.rs", 1, 0)], &[], 50));
    }

    #[test]
    fn critical_rule_forces_full_review() {
        let targets = [target("src/a.rs", 1, 0)];
        let critical = rule(Severity::Critical, Some("src/**"), true);
        assert!(!is_small_pr(&targets, &[critical], 50));
        // Not covered, not critical or disabled: still small.
        let elsewhere = rule(Severity::Critical, Some("docs/**"), true);
        let warning = rule(Severity::Warning, None, true);
        let disabled = rule(Severity::Critical, None, false);
        assert!(is_small_pr(&targets, &[elsewhere, warning, disabled], 50));
    }
}
