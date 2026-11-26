//! Per-hunk record of which changed lines a Finder run has opened with `read_file`.
//!
//! The ledger drives the coverage debt note and the one-shot completion gate: a file is only
//! "read" once every right-side hunk of its diff lies inside lines the model actually saw.

use std::collections::{BTreeMap, HashSet};

use adus_core::types::normalize_path;
use adus_git::diff::commentable_hunks;
use adus_git::{FileDiff, FileStatus};

/// Files shown in one debt note.
const COVERAGE_DEBT_MAX_FILES: usize = 8;
/// Pending ranges shown per file in one debt note.
const COVERAGE_DEBT_MAX_RANGES: usize = 3;
/// Extensions of documentation files, which carry no read debt.
const GATE_DOC_EXTENSIONS: [&str; 4] = ["md", "mdx", "rst", "txt"];

/// One changed file: its right-side hunks and the line windows read so far.
#[derive(Debug, Default)]
struct Target {
    /// Inclusive right-side hunk ranges. Empty for a file without commentable hunks.
    hunks: Vec<(u32, u32)>,
    /// Sorted, merged, inclusive windows the model has seen.
    reads: Vec<(u32, u32)>,
    /// Set by any successful read; covers a file without hunks.
    read: bool,
    /// Set by a read of an empty file (`0-0`), which covers every hunk.
    whole: bool,
}

impl Target {
    fn add_read(&mut self, start: u32, end: u32) {
        self.read = true;
        if end == 0 {
            self.whole = true;
            return;
        }
        self.reads.push((start, end));
        self.reads.sort_unstable();
        let mut merged: Vec<(u32, u32)> = Vec::with_capacity(self.reads.len());
        for &(cur_start, cur_end) in &self.reads {
            match merged.last_mut() {
                Some(prev) if cur_start <= prev.1.saturating_add(1) => {
                    prev.1 = prev.1.max(cur_end);
                }
                _ => merged.push((cur_start, cur_end)),
            }
        }
        self.reads = merged;
    }

    /// Hunks not yet inside one read window, in line order.
    fn pending_ranges(&self) -> Vec<(u32, u32)> {
        if self.whole {
            return Vec::new();
        }
        self.hunks
            .iter()
            .copied()
            .filter(|&(hunk_start, hunk_end)| {
                !self
                    .reads
                    .iter()
                    .any(|&(start, end)| start <= hunk_start && hunk_end <= end)
            })
            .collect()
    }

    fn is_pending(&self) -> bool {
        if self.hunks.is_empty() {
            !self.read
        } else {
            !self.pending_ranges().is_empty()
        }
    }
}

/// Tracks, per changed file, which hunks have been read.
#[derive(Debug, Default)]
pub(crate) struct CoverageLedger {
    targets: BTreeMap<String, Target>,
    opened: HashSet<String>,
}

impl CoverageLedger {
    /// One target per readable file of the batch. A deleted file has no head version to open,
    /// so it carries no debt.
    pub(crate) fn from_files(files: &[&FileDiff]) -> Self {
        let targets = files
            .iter()
            .filter(|file| file.status != FileStatus::Deleted)
            .map(|file| {
                let hunks = file
                    .patch
                    .as_deref()
                    .map(commentable_hunks)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|range| (*range.start(), *range.end()))
                    .collect();
                (
                    normalize_path(&file.path),
                    Target {
                        hunks,
                        ..Target::default()
                    },
                )
            })
            .collect();
        Self {
            targets,
            opened: HashSet::new(),
        }
    }

    /// Records a successful `read_file` that showed lines `start..=end` (`0-0`: an empty file).
    pub(crate) fn record_read(&mut self, rel_path: &str, start: u32, end: u32) {
        if end != 0 && end < start {
            return;
        }
        let path = normalize_path(rel_path);
        if let Some(target) = self.targets.get_mut(&path) {
            target.add_read(start, end);
        }
        self.opened.insert(path);
    }

    /// Every file with at least one successful read, tracked or not.
    pub(crate) fn opened_files(&self) -> HashSet<String> {
        self.opened.clone()
    }

    /// Sorted paths of qualifying files (not docs, not tests) with an unread hunk.
    pub(crate) fn pending_qualifying_files(&self) -> Vec<String> {
        self.pending_qualifying()
            .map(|(path, _)| path.to_owned())
            .collect()
    }

    /// Qualifying files with all their pending ranges, sorted by path. A new debt note is due
    /// only when this differs from the signature of the last note sent.
    pub(crate) fn pending_signature(&self) -> Vec<(String, Vec<(u32, u32)>)> {
        self.pending_qualifying()
            .map(|(path, target)| (path.to_owned(), target.pending_ranges()))
            .collect()
    }

    /// The text asking the model to read what it skipped, or `None` when nothing qualifying is
    /// pending. `submit_name` is the tool the model must not call yet.
    pub(crate) fn debt_note(&self, submit_name: &str) -> Option<String> {
        let files: Vec<String> = self
            .pending_qualifying()
            .take(COVERAGE_DEBT_MAX_FILES)
            .map(|(path, target)| format!("- {}", file_debt(path, &target.pending_ranges())))
            .collect();
        if files.is_empty() {
            return None;
        }
        Some(format!(
            "Unread changed code. You have not opened these changed lines with read_file yet:\n\
             {}\n\
             Read them before you call {submit_name}. A grep or list_dir match does not count as reading them.",
            files.join("\n")
        ))
    }

    fn pending_qualifying(&self) -> impl Iterator<Item = (&str, &Target)> {
        self.targets
            .iter()
            .filter(|(path, target)| qualifies(path) && target.is_pending())
            .map(|(path, target)| (path.as_str(), target))
    }
}

/// `path: lines a-b, c-d`; a lone one-line range reads `line a`; more than the cap ends with
/// `, …`. A file without commentable hunks is named alone.
fn file_debt(path: &str, ranges: &[(u32, u32)]) -> String {
    let Some(&(first_start, first_end)) = ranges.first() else {
        return path.to_owned();
    };
    let shown: Vec<String> = ranges
        .iter()
        .take(COVERAGE_DEBT_MAX_RANGES)
        .map(|&(start, end)| {
            if start == end {
                start.to_string()
            } else {
                format!("{start}-{end}")
            }
        })
        .collect();
    let label = if ranges.len() == 1 && first_start == first_end {
        "line"
    } else {
        "lines"
    };
    let more = if ranges.len() > COVERAGE_DEBT_MAX_RANGES {
        ", …"
    } else {
        ""
    };
    format!("{path}: {label} {}{more}", shown.join(", "))
}

/// A file the gate requires reading: neither documentation nor a test.
fn qualifies(path: &str) -> bool {
    !is_doc_path(path) && !is_test_path(path)
}

fn is_doc_path(path: &str) -> bool {
    path.rsplit_once('.').is_some_and(|(_, ext)| {
        GATE_DOC_EXTENSIONS
            .iter()
            .any(|doc| ext.eq_ignore_ascii_case(doc))
    })
}

/// `*_test.*`, `*.test.*`, `*.spec.*`, or any file under a `tests` or `__tests__` directory.
fn is_test_path(path: &str) -> bool {
    let path = path.to_ascii_lowercase();
    let mut segments = path.rsplit('/');
    let name = segments.next().unwrap_or_default();
    name.contains("_test.")
        || name.contains(".test.")
        || name.contains(".spec.")
        || segments.any(|dir| dir == "tests" || dir == "__tests__")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::parse_read_header_range;

    fn diff(path: &str, hunks: &[(u32, u32)]) -> FileDiff {
        let patch: String = hunks
            .iter()
            .map(|&(start, end)| {
                let count = end - start + 1;
                let body = "+x\n".repeat(count as usize);
                format!("@@ -{start},0 +{start},{count} @@\n{body}")
            })
            .collect();
        FileDiff {
            path: path.to_owned(),
            old_path: None,
            status: FileStatus::Modified,
            patch: Some(patch),
        }
    }

    fn ledger(files: &[FileDiff]) -> CoverageLedger {
        CoverageLedger::from_files(&files.iter().collect::<Vec<_>>())
    }

    #[test]
    fn coverage_ledger_tracks_per_hunk_unions_and_ignores_truncated_unread_lines() {
        let mut two = ledger(&[diff("src/a.rs", &[(1, 10), (50, 60)])]);
        two.record_read("src/a.rs", 1, 20);
        assert_eq!(two.pending_qualifying_files(), ["src/a.rs"]);
        let note = two.debt_note("submit_findings").unwrap();
        assert!(note.contains("- src/a.rs: lines 50-60\n"), "{note}");
        assert!(note.ends_with(
            "Read them before you call submit_findings. A grep or list_dir match does not count as reading them."
        ));
        two.record_read("src/a.rs", 45, 65);
        assert!(two.pending_qualifying_files().is_empty());
        assert_eq!(two.debt_note("submit_findings"), None);

        // Adjacent reads merge into one window.
        let mut adjacent = ledger(&[diff("src/a.rs", &[(1, 10)])]);
        adjacent.record_read("src/a.rs", 1, 5);
        assert!(!adjacent.pending_qualifying_files().is_empty());
        adjacent.record_read("src/a.rs", 6, 10);
        assert!(adjacent.pending_qualifying_files().is_empty());

        // A truncated read records the lines shown (header range), not the lines requested.
        let (start, end) = parse_read_header_range("[src/a.rs: lines 1-8 of 80]\n1: x").unwrap();
        let mut truncated = ledger(&[diff("src/a.rs", &[(1, 10)])]);
        truncated.record_read("src/a.rs", start, end);
        assert_eq!(
            truncated.pending_signature(),
            [("src/a.rs".to_owned(), vec![(1, 10)])]
        );

        // An empty file (`0-0`) covers every hunk.
        let mut empty = ledger(&[diff("src/a.rs", &[(1, 10), (50, 60)])]);
        empty.record_read("src/a.rs", 0, 0);
        assert!(empty.pending_qualifying_files().is_empty());
    }

    #[test]
    fn coverage_ledger_excludes_docs_and_tests_and_formats_the_note() {
        let mut ledger = ledger(&[
            diff("README.MD", &[(1, 3)]),
            diff("docs/guide.txt", &[(1, 3)]),
            diff("internal/image/helpers_test.go", &[(1, 3)]),
            diff("src/foo.test.ts", &[(1, 3)]),
            diff("tests/e2e.rs", &[(1, 3)]),
            diff("src/b.rs", &[(7, 7), (20, 21), (30, 31), (40, 41)]),
        ]);
        assert_eq!(ledger.pending_qualifying_files(), ["src/b.rs"]);
        let note = ledger.debt_note("submit_findings").unwrap();
        assert!(
            note.contains("- src/b.rs: lines 7, 20-21, 30-31, …\n"),
            "{note}"
        );
        assert!(
            !note.contains("README") && !note.contains("tests/"),
            "{note}"
        );

        ledger.record_read("src/b.rs", 1, 50);
        assert!(ledger.pending_signature().is_empty());
        assert_eq!(ledger.debt_note("submit_findings"), None);
    }

    #[test]
    fn coverage_ledger_names_single_lines_and_hunkless_files_and_signature_tracks_covered_ranges() {
        let mut ledger = ledger(&[
            diff("src/a.rs", &[(1, 10), (50, 60)]),
            diff("src/c.rs", &[(7, 7)]),
            FileDiff {
                path: "assets/logo.bin".to_owned(),
                old_path: None,
                status: FileStatus::Modified,
                patch: None,
            },
        ]);
        let note = ledger.debt_note("submit_findings").unwrap();
        assert!(note.contains("- assets/logo.bin\n"), "{note}");
        assert!(note.contains("- src/c.rs: line 7\n"), "{note}");

        let before = ledger.pending_signature();
        ledger.record_read("src/a.rs", 100, 120);
        ledger.record_read("src/other.rs", 1, 50);
        assert_eq!(ledger.pending_signature(), before);
        assert!(ledger.opened_files().contains("src/other.rs"));
        ledger.record_read("src/a.rs", 1, 10);
        assert_ne!(ledger.pending_signature(), before);
    }
}
