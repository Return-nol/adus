//! The PR's existing discussion as model input: which threads to send and how to render them.
//! Comment bodies are untrusted: they are redacted and cut here, and callers wrap the result
//! with `prompt::wrap`.

use std::collections::BTreeSet;

use adus_git::PrThread;
use serde_json::{Value, json};

use crate::prompt::truncate_with_marker;
use crate::redact::redact_secrets;

/// Most threads sent to Semantic Dedup.
const MAX_DEDUP_THREADS: usize = 60;
/// Longest comment body sent to the model, in chars.
pub(crate) const MAX_BODY_CHARS: usize = 1_000;
/// Longest formatted threads section (`format_threads`) sent to the model, in chars.
pub(crate) const MAX_THREADS_CHARS: usize = 40_000;

/// Threads on a file in `paths`, then general threads (no path), each group newest first, at
/// most `MAX_DEDUP_THREADS`, cut by [`within_budget`]. Threads carry no timestamp, so "newest"
/// is the reverse of the provider's order.
pub(crate) fn threads_for_dedup<'a>(
    threads: &'a [PrThread],
    paths: &BTreeSet<String>,
) -> Vec<&'a PrThread> {
    let same_file = threads
        .iter()
        .rev()
        .filter(|t| t.path.as_ref().is_some_and(|p| paths.contains(p)));
    let general = threads.iter().rev().filter(|t| t.path.is_none());
    within_budget(same_file.chain(general).take(MAX_DEDUP_THREADS))
}

/// The `threads` (already in priority order) whose `format_threads` output stays within
/// `MAX_THREADS_CHARS`; any thread that would push the output over the budget is skipped so
/// smaller subsequent threads can still fit.
// ponytail: re-formats the list per thread (quadratic), fine for at most 60 threads.
pub(crate) fn within_budget<'a>(
    threads: impl IntoIterator<Item = &'a PrThread>,
) -> Vec<&'a PrThread> {
    let mut fitted = Vec::new();
    for thread in threads {
        fitted.push(thread);
        if format_threads(&fitted).chars().count() > MAX_THREADS_CHARS {
            fitted.pop();
            continue;
        }
    }
    fitted
}

/// `threads` as a JSON array: each thread with its `index`, `path`, `line`, `status` and its
/// comments (`index`, `author` — `"adus"` for Adus's own — and a redacted, cut `body`).
pub(crate) fn format_threads(threads: &[&PrThread]) -> String {
    let items: Vec<Value> = threads
        .iter()
        .enumerate()
        .map(|(index, thread)| {
            let comments: Vec<Value> = thread
                .comments
                .iter()
                .enumerate()
                .map(|(index, comment)| {
                    let author = if comment.by_adus {
                        "adus"
                    } else {
                        &comment.author
                    };
                    let body = redact_secrets(&comment.body);
                    let body = truncate_with_marker(&body, MAX_BODY_CHARS, " …");
                    json!({"index": index, "author": author, "body": body})
                })
                .collect();
            json!({
                "index": index,
                "path": thread.path,
                "line": thread.line,
                "status": thread.status,
                "comments": comments,
            })
        })
        .collect();
    serde_json::to_string_pretty(&items).unwrap_or_else(|_| Value::Array(items).to_string())
}

#[cfg(test)]
mod tests {
    use adus_git::PrThreadComment;

    use super::*;

    fn thread(path: Option<&str>, body: &str) -> PrThread {
        PrThread {
            id: None,
            resolved: false,
            path: path.map(str::to_string),
            line: path.map(|_| 2),
            status: None,
            url: None,
            comments: vec![PrThreadComment {
                id: String::new(),
                author: "alice".into(),
                by_adus: false,
                body: body.into(),
            }],
        }
    }

    #[test]
    fn threads_for_dedup_filters_orders_and_caps() {
        let mut threads = vec![thread(None, "general")];
        threads.extend((0..70).map(|i| thread(Some("src/a.rs"), &format!("same {i}"))));
        threads.push(thread(Some("src/other.rs"), "other file"));
        let paths = BTreeSet::from(["src/a.rs".to_string()]);

        let picked = threads_for_dedup(&threads, &paths);

        assert_eq!(picked.len(), 60);
        assert!(picked.iter().all(|t| t.path.as_deref() == Some("src/a.rs")));
        assert_eq!(picked[0].comments[0].body, "same 69", "newest first");

        let few = threads_for_dedup(&threads[..3], &paths);
        let bodies: Vec<&str> = few.iter().map(|t| t.comments[0].body.as_str()).collect();
        assert_eq!(bodies, ["same 1", "same 0", "general"]);

        // 50 threads of about 1,100 formatted chars each exceed the 40,000-char budget.
        let long: Vec<PrThread> = (0..50)
            .map(|i| thread(Some("src/a.rs"), &format!("{i:02}{}", "y".repeat(998))))
            .collect();
        let picked = threads_for_dedup(&long, &paths);
        assert!(picked.len() < 50);
        assert_eq!(picked[0].comments[0].body[..2], *"49", "newest kept");
        let formatted = format_threads(&picked).chars().count();
        assert!(formatted <= MAX_THREADS_CHARS, "{formatted}");
        let mut one_more = picked.clone();
        one_more.push(&long[49 - picked.len()]);
        assert!(format_threads(&one_more).chars().count() > MAX_THREADS_CHARS);
    }

    #[test]
    fn format_threads_marks_adus_and_cuts() {
        let mut long = thread(Some("src/a.rs"), &"x".repeat(1_500));
        long.comments.push(PrThreadComment {
            id: String::new(),
            author: "pat-user".into(),
            by_adus: true,
            body: "earlier finding".into(),
        });
        let out = format_threads(&[&long]);
        assert!(out.contains("\"author\": \"adus\""));
        assert!(out.contains("\"author\": \"alice\""));
        assert!(out.contains(&format!("{} …", "x".repeat(1_000))));
        assert!(!out.contains(&"x".repeat(1_001)));
    }

    #[test]
    fn within_budget_skips_oversized_thread() {
        let small1 = thread(Some("src/a.rs"), "first small");
        let mut oversized = thread(Some("src/a.rs"), &"z".repeat(MAX_BODY_CHARS));
        oversized.comments = (0..45)
            .map(|_| PrThreadComment {
                id: String::new(),
                author: "alice".into(),
                by_adus: false,
                body: "z".repeat(MAX_BODY_CHARS),
            })
            .collect();
        let small2 = thread(Some("src/b.rs"), "second small");

        let picked = within_budget([&small1, &oversized, &small2]);
        let bodies: Vec<&str> = picked.iter().map(|t| t.comments[0].body.as_str()).collect();
        assert_eq!(bodies, ["first small", "second small"]);
    }
}
