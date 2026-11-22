mod common;

use std::time::{Duration, Instant};

use adus_core::job::Trigger;
use adus_core::types::{ADUDU_MARKER, ReviewMode, Severity};
use adus_db::connections::{self, ConnectionStatus};
use adus_db::repositories::{self, RepoPatch};
use adus_db::reviews::{self, Review, ReviewStatus};
use adus_db::rules::{self, KnowledgeScope, NewRule};
use adus_db::settings;
use adus_git::{ConnectionKind, FileStatus, GitError};
use adus_llm::ReasoningEffort;
use adus_llm::router::ChatError;
use adus_review::engine::{EngineError, REVIEW_TIMEOUT, SPECIALISTS_REVIEW_TIMEOUT};
use adus_review::finder::FINDER_MAX_STEPS;
use adus_review::workspace::WorkspaceError;
use common::{
    CloneBehavior, FakeGit, PAT, Reply, Setup, Stage, big_file, clone_spec, diff_header, file,
    finding, pr, setup,
};
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

/// Tiga baris ditambah: 2, 3, 4.
const PATCH_234: &str = "@@ -1,1 +1,4 @@\n a\n+b\n+c\n+d\n";
/// Satu baris ditambah: 2.
const PATCH_2: &str = "@@ -1,1 +1,2 @@\n a\n+b\n";

const GENERALIST_ROLE: &str = "You are the generalist reviewer";
const BUG_ROLE: &str = "You are the correctness reviewer";
const SECURITY_ROLE: &str = "You are the security reviewer";
const PERFORMANCE_ROLE: &str = "You are the performance reviewer";
const RULES_ROLE: &str = "You are the team-rules reviewer";

const CODEBASE_NOTE: &str = "Codebase context unavailable — reviewed diff only";
const TOOLS_NOTE: &str = "Model does not support tool calling — reviewed diff only";

async fn review(pool: &PgPool, id: Uuid) -> Review {
    reviews::list(pool, None, 1, 100)
        .await
        .unwrap()
        .into_iter()
        .find(|r| r.id == id)
        .expect("review row")
}

async fn set_limits(pool: &PgPool, max_files: i32, max_lines: i32) {
    let mut current = settings::get(pool).await.unwrap();
    current.max_files_per_review = max_files;
    current.max_patch_lines_per_file = max_lines;
    settings::update(pool, &current).await.unwrap();
}

/// Review PR 7 di repo hasil `setup`; repo itu ada, jadi selalu ada baris review.
async fn run(s: &Setup, trigger: Trigger) -> Result<Uuid, EngineError> {
    run_mode(s, trigger, None).await
}

async fn run_mode(
    s: &Setup,
    trigger: Trigger,
    mode: Option<ReviewMode>,
) -> Result<Uuid, EngineError> {
    let result = s.engine.review_pr(s.repository_id, 7, trigger, mode).await;
    result.map(|id| id.expect("review row"))
}

async fn latest(pool: &PgPool) -> Review {
    reviews::list(pool, None, 1, 1).await.unwrap().remove(0)
}

/// A global rule for `src/**`.
async fn insert_rule(pool: &PgPool) {
    rules::insert(
        pool,
        NewRule {
            scope: KnowledgeScope::Global,
            repository_id: None,
            title: "No unwrap",
            body: "Use ?",
            path_glob: Some("src/**"),
            severity: Severity::Critical,
        },
    )
    .await
    .unwrap();
}

/// One finding as a JSON object for `submit_findings`.
fn finding_json(path: &str, line: u32, severity: &str, title: &str) -> serde_json::Value {
    json!({"file": path, "line": line, "severity": severity, "title": title, "body": "why"})
}

#[sqlx::test(migrations = "../../migrations")]
async fn full_review_posts_valid_comments_and_summary(pool: PgPool) {
    let s = setup(
        &pool,
        FakeGit::new(pr("h1"), vec![file("src/a.rs", PATCH_234)]),
    )
    .await;
    insert_rule(&pool).await;
    let both = r#"[{"file":"src/a.rs","line":3,"severity":"critical","title":"Bug","body":"why"},
                   {"file":"src/a.rs","line":1,"severity":"info","title":"Context line","body":"x"}]"#;
    s.chat.push(Ok(both));

    let id = run(&s, Trigger::Opened).await.unwrap();

    let row = review(&pool, id).await;
    assert_eq!(row.status, ReviewStatus::Done);
    assert_eq!(row.comments_posted, 1);
    assert_eq!(row.head_sha.as_deref(), Some("h1"));
    assert_eq!(row.base_sha.as_deref(), Some("base"));
    assert_eq!(row.trigger, Trigger::Opened);

    let git = s.git.state();
    assert_eq!(
        git.created,
        vec![(ConnectionKind::Github, "acme".to_string(), PAT.to_string())]
    );
    assert_eq!(git.reviews.len(), 1);
    let posted = &git.reviews[0];
    assert_eq!(posted.comments.len(), 1, "line 1 is context, dropped");
    assert_eq!(posted.comments[0].path, "src/a.rs");
    assert_eq!(posted.comments[0].line, 3);
    assert!(posted.comments[0].body.starts_with(ADUDU_MARKER));
    assert!(posted.comments[0].body.contains("Bug"));
    assert!(posted.summary.contains(ADUDU_MARKER));
    assert!(posted.summary.contains("Critical: 1"));
    assert!(!posted.summary.contains("Info"));
    assert!(!posted.summary.contains("**Notes**"));

    let finder = s.chat.finder_calls();
    assert_eq!(finder.len(), 1);
    assert_eq!(finder[0].provider_id, s.provider_id);
    assert_eq!(finder[0].model, "gpt-test");
    assert_eq!(finder[0].max_tokens, 8192);
    assert!(finder[0].user.contains("No unwrap"));
    // The one valid finding is verified with the fast model; one finding needs no dedup.
    let verifier = s.chat.verifier_calls();
    assert_eq!(verifier.len(), 1);
    assert_eq!(verifier[0].provider_id, s.provider_id);
    assert_eq!(verifier[0].model, "gpt-fast");
    assert_eq!(verifier[0].effort, Some(ReasoningEffort::Low));
    assert!(verifier[0].user.contains("\"title\": \"Bug\""));
    assert!(s.chat.dedup_calls().is_empty());
    assert_eq!(s.chat.calls().len(), 2);
}

#[sqlx::test(migrations = "../../migrations")]
async fn draft_pr_skipped_unless_command(pool: PgPool) {
    let mut draft = pr("h1");
    draft.is_draft = true;
    let s = setup(&pool, FakeGit::new(draft, vec![file("a.rs", PATCH_2)])).await;

    for trigger in [Trigger::Opened, Trigger::Updated] {
        let id = run(&s, trigger).await.unwrap();
        let row = review(&pool, id).await;
        assert_eq!(row.status, ReviewStatus::Skipped);
        assert_eq!(row.skip_reason.as_deref(), Some("draft"));
    }
    assert!(s.chat.calls().is_empty());
    assert!(s.git.state().reviews.is_empty());

    let id = run(&s, Trigger::Command).await.unwrap();
    assert_eq!(review(&pool, id).await.status, ReviewStatus::Done);
    assert_eq!(s.git.state().reviews.len(), 1);
}

#[sqlx::test(migrations = "../../migrations")]
async fn auto_review_off_skips_opened_but_runs_command(pool: PgPool) {
    let s = setup(&pool, FakeGit::new(pr("h1"), vec![file("a.rs", PATCH_2)])).await;
    let patch = RepoPatch {
        auto_review: Some(false),
        ..Default::default()
    };
    repositories::update(&pool, s.repository_id, patch)
        .await
        .unwrap();

    let id = run(&s, Trigger::Opened).await.unwrap();
    let row = review(&pool, id).await;
    assert_eq!(row.status, ReviewStatus::Skipped);
    assert_eq!(row.skip_reason.as_deref(), Some("auto_review_off"));
    assert!(s.git.state().created.is_empty(), "nothing fetched");

    let id = run(&s, Trigger::Command).await.unwrap();
    assert_eq!(review(&pool, id).await.status, ReviewStatus::Done);
    assert_eq!(s.git.state().reviews.len(), 1);
}

#[sqlx::test(migrations = "../../migrations")]
async fn disabled_or_missing_repo_is_skipped(pool: PgPool) {
    let s = setup(&pool, FakeGit::new(pr("h1"), vec![file("a.rs", PATCH_2)])).await;
    let patch = RepoPatch {
        enabled: Some(false),
        ..Default::default()
    };
    repositories::update(&pool, s.repository_id, patch)
        .await
        .unwrap();

    let id = run(&s, Trigger::Command).await.unwrap();
    let row = review(&pool, id).await;
    assert_eq!(row.status, ReviewStatus::Skipped);
    assert_eq!(row.skip_reason.as_deref(), Some("repository_disabled"));

    let missing = s
        .engine
        .review_pr(Uuid::new_v4(), 7, Trigger::Command, None)
        .await;
    assert_eq!(missing, Ok(None));
    assert!(s.git.state().created.is_empty());
}

#[sqlx::test(migrations = "../../migrations")]
async fn incremental_review_only_new_commit_lines(pool: PgPool) {
    let s = setup(&pool, FakeGit::new(pr("h1"), vec![file("a.rs", PATCH_2)])).await;
    run(&s, Trigger::Opened).await.unwrap();

    {
        let mut git = s.git.state();
        git.pr = pr("h2");
        // Full PR diff: a.rs lines 2-4, b.rs line 2.
        git.full = vec![file("a.rs", PATCH_234), file("b.rs", PATCH_2)];
        // New commit: a.rs line 4 and line 9 (9 is not in the full PR diff), c.rs (reverted later).
        git.incremental = vec![
            file("a.rs", "@@ -3,1 +3,2 @@\n c\n+d\n@@ -8,1 +9,1 @@\n-x\n+y\n"),
            file("c.rs", PATCH_2),
        ];
    }
    let answer = r#"[{"file":"a.rs","line":2,"title":"old line","body":"x"},
                     {"file":"a.rs","line":4,"title":"new line","body":"x"},
                     {"file":"a.rs","line":9,"title":"not in PR","body":"x"}]"#;
    s.chat.push(Ok(answer));

    let id = run(&s, Trigger::Updated).await.unwrap();

    assert_eq!(review(&pool, id).await.status, ReviewStatus::Done);
    let git = s.git.state();
    assert_eq!(
        git.diff_between_calls,
        vec![("h1".to_string(), "h2".to_string())]
    );
    let calls = s.chat.finder_calls();
    assert_eq!(calls.len(), 2, "first run 1 file, incremental run 1 file");
    assert!(calls[1].user.contains(&diff_header("a.rs")));
    assert!(!calls[1].user.contains(&diff_header("b.rs")));
    assert!(!calls[1].user.contains(&diff_header("c.rs")));
    assert!(
        calls[1].user.contains("4 + d"),
        "prompt has the new-commit patch"
    );
    // Only the finding on a new line reaches the Verifier.
    assert_eq!(s.chat.verifier_calls().len(), 1);
    let comments = &git.reviews[1].comments;
    assert_eq!(comments.len(), 1);
    assert_eq!((comments[0].path.as_str(), comments[0].line), ("a.rs", 4));
}

#[sqlx::test(migrations = "../../migrations")]
async fn same_head_sha_skipped(pool: PgPool) {
    let s = setup(&pool, FakeGit::new(pr("h1"), vec![file("a.rs", PATCH_2)])).await;
    run(&s, Trigger::Opened).await.unwrap();

    let id = run(&s, Trigger::Updated).await.unwrap();

    let row = review(&pool, id).await;
    assert_eq!(row.status, ReviewStatus::Skipped);
    assert_eq!(row.skip_reason.as_deref(), Some("already_reviewed"));
    assert_eq!(s.chat.calls().len(), 1);
    let git = s.git.state();
    assert_eq!(git.reviews.len(), 1);
    assert!(git.diff_between_calls.is_empty());
}

#[sqlx::test(migrations = "../../migrations")]
async fn large_and_binary_files_skipped_with_reason_in_summary(pool: PgPool) {
    let mut binary = file("logo.png", "");
    binary.patch = None;
    let mut deleted = file("gone.rs", "@@ -1,1 +0,0 @@\n-a\n");
    deleted.status = FileStatus::Deleted;
    let files = vec![
        file("big.rs", PATCH_234),
        binary,
        deleted,
        file("ok.rs", PATCH_2),
    ];
    let s = setup(&pool, FakeGit::new(pr("h1"), files)).await;
    set_limits(&pool, 50, 2).await;
    // Daftar bawaan mengabaikan *.png; test ini menguji jalur `no_patch`.
    set_ignore(&pool, Some(&[]), &[]).await;

    run(&s, Trigger::Opened).await.unwrap();

    let calls = s.chat.finder_calls();
    assert_eq!(calls.len(), 1);
    assert!(calls[0].user.contains(&diff_header("ok.rs")));
    for path in ["big.rs", "logo.png", "gone.rs"] {
        assert!(!calls[0].user.contains(&diff_header(path)), "{path}");
    }
    let summary = s.git.state().reviews[0].summary.clone();
    assert!(summary.contains("`big.rs`: patch too large"));
    assert!(summary.contains("`logo.png`: no diff available (binary or too large)"));
    assert!(!summary.contains("gone.rs"));
}

#[sqlx::test(migrations = "../../migrations")]
async fn only_skipped_files_posts_summary_without_llm_or_clone(pool: PgPool) {
    let s = setup(
        &pool,
        FakeGit::new(pr("h1"), vec![file("big.rs", PATCH_234)]),
    )
    .await;
    s.git.state().clone_spec = Some(clone_spec());
    set_limits(&pool, 50, 2).await;

    let id = run(&s, Trigger::Opened).await.unwrap();

    let row = review(&pool, id).await;
    assert_eq!(row.status, ReviewStatus::Done);
    assert_eq!(row.comments_posted, 0);
    assert!(s.chat.calls().is_empty());
    assert!(
        s.cloner.urls().is_empty(),
        "nothing to review, nothing cloned"
    );
    let summary = s.git.state().reviews[0].summary.clone();
    assert!(summary.contains("No issues found."));
    assert!(summary.contains("`big.rs`: patch too large"));
}

#[sqlx::test(migrations = "../../migrations")]
async fn file_limit_respected(pool: PgPool) {
    let files = vec![
        file("a.rs", PATCH_2),
        file("b.rs", PATCH_2),
        file("c.rs", PATCH_2),
    ];
    let s = setup(&pool, FakeGit::new(pr("h1"), files)).await;
    set_limits(&pool, 2, 2000).await;

    run(&s, Trigger::Opened).await.unwrap();

    // The two small files share one Finder batch; c.rs is not sent.
    let calls = s.chat.finder_calls();
    assert_eq!(calls.len(), 1);
    assert!(calls[0].user.contains(&diff_header("a.rs")));
    assert!(calls[0].user.contains(&diff_header("b.rs")));
    assert!(!calls[0].user.contains(&diff_header("c.rs")));
    let summary = s.git.state().reviews[0].summary.clone();
    assert!(summary.contains("`c.rs`: file limit reached"));
    assert!(!summary.contains("`a.rs`"));
}

#[sqlx::test(migrations = "../../migrations")]
async fn unparseable_output_retried_once_then_skipped(pool: PgPool) {
    // Each big file gets its own Finder batch.
    let files = vec![big_file("a.rs"), big_file("b.rs")];
    let s = setup(&pool, FakeGit::new(pr("h1"), files)).await;
    let a = diff_header("a.rs");
    let b = diff_header("b.rs");
    s.chat
        .push_for(Stage::Finder, &a, Reply::text("I think it's fine"));
    s.chat.push_for(
        Stage::Finder,
        &a,
        Reply::text(&finding("a.rs", 2, "Second try")),
    );
    s.chat.push_for(Stage::Finder, &b, Reply::text("nope"));
    s.chat
        .push_for(Stage::Finder, &b, Reply::text("still nope"));

    let id = run(&s, Trigger::Opened).await.unwrap();

    let finder = s.chat.finder_calls();
    assert_eq!(finder.len(), 4);
    assert_eq!(finder.iter().filter(|c| c.user.contains(&a)).count(), 2);
    assert_eq!(finder.iter().filter(|c| c.user.contains(&b)).count(), 2);
    assert_eq!(review(&pool, id).await.comments_posted, 1);
    let git = s.git.state();
    assert!(git.reviews[0].comments[0].body.contains("Second try"));
    assert!(
        git.reviews[0]
            .summary
            .contains("`b.rs`: review output could not be parsed")
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn all_keys_failed_is_retryable_and_posts_nothing(pool: PgPool) {
    let files = vec![big_file("a.rs"), big_file("b.rs")];
    let s = setup(&pool, FakeGit::new(pr("h1"), files)).await;
    for path in ["a.rs", "b.rs"] {
        s.chat.push_for(
            Stage::Finder,
            &diff_header(path),
            Reply::Err(ChatError::AllKeysFailed {
                retry_after: Some(Duration::from_secs(30)),
            }),
        );
    }

    let err = run(&s, Trigger::Opened).await.unwrap_err();

    match err {
        EngineError::Retryable(_, after) => assert_eq!(after, Some(Duration::from_secs(30))),
        other => panic!("expected Retryable, got {other:?}"),
    }
    {
        let git = s.git.state(); // lepas lock sebelum run berikutnya
        assert!(git.reviews.is_empty());
        assert!(git.comments.is_empty());
    }
    // Retryable: barisnya dihapus, percobaan ulang membuat baris baru.
    assert!(reviews::list(&pool, None, 1, 10).await.unwrap().is_empty());

    s.git.state().full = vec![file("a.rs", PATCH_2)];
    s.chat.push(Err(ChatError::NoKeys));
    assert!(matches!(
        run(&s, Trigger::Opened).await,
        Err(EngineError::Retryable(_, None))
    ));
    assert!(reviews::list(&pool, None, 1, 10).await.unwrap().is_empty());
    assert!(s.git.state().reviews.is_empty());
}

/// Spec decision 8, Generalist rule: when every failed Finder task failed retryably, the whole
/// review is retried instead of posting the other batches.
#[sqlx::test(migrations = "../../migrations")]
async fn generalist_rate_limited_batch_retries_the_review(pool: PgPool) {
    let files = vec![big_file("a.rs"), big_file("b.rs"), big_file("c.rs")];
    let s = setup(&pool, FakeGit::new(pr("h1"), files)).await;
    s.chat.push_for(
        Stage::Finder,
        &diff_header("a.rs"),
        Reply::Err(ChatError::NoKeys),
    );
    s.chat.push_for(
        Stage::Finder,
        &diff_header("b.rs"),
        Reply::Err(ChatError::AllKeysFailed {
            retry_after: Some(Duration::from_secs(45)),
        }),
    );
    s.chat.push_for(
        Stage::Finder,
        &diff_header("c.rs"),
        Reply::text(&finding("c.rs", 2, "Bug")),
    );

    let err = run(&s, Trigger::Opened).await.unwrap_err();

    // The first `AllKeysFailed` sets the retry delay.
    assert_eq!(
        err,
        EngineError::Retryable(
            "LLM: all API keys failed".into(),
            Some(Duration::from_secs(45))
        )
    );
    assert!(s.git.state().reviews.is_empty(), "nothing posted");
    assert!(s.chat.verifier_calls().is_empty());
    assert!(reviews::list(&pool, None, 1, 10).await.unwrap().is_empty());
}

/// A non-retryable failure among the failed tasks keeps the partial post in Generalist mode.
#[sqlx::test(migrations = "../../migrations")]
async fn generalist_mixed_failures_post_the_rest_with_skipped_files(pool: PgPool) {
    let files = vec![big_file("a.rs"), big_file("b.rs"), big_file("c.rs")];
    let s = setup(&pool, FakeGit::new(pr("h1"), files)).await;
    s.chat.push_for(
        Stage::Finder,
        &diff_header("a.rs"),
        Reply::Err(ChatError::AllKeysFailed { retry_after: None }),
    );
    s.chat.push_for(
        Stage::Finder,
        &diff_header("b.rs"),
        Reply::Err(ChatError::Invalid("bad request".into())),
    );
    s.chat.push_for(
        Stage::Finder,
        &diff_header("c.rs"),
        Reply::text(&finding("c.rs", 2, "Bug")),
    );

    let id = run(&s, Trigger::Opened).await.unwrap();

    let row = review(&pool, id).await;
    assert_eq!(row.status, ReviewStatus::Done);
    assert_eq!(row.comments_posted, 1);
    let git = s.git.state();
    assert_eq!(git.reviews[0].comments[0].path, "c.rs");
    let summary = &git.reviews[0].summary;
    assert!(summary.contains("`a.rs`: LLM error"), "{summary}");
    assert!(summary.contains("`b.rs`: LLM error"), "{summary}");
}

/// Spec decision 8, Specialists: a rate-limited specialist is a note; the survivors are posted.
#[sqlx::test(migrations = "../../migrations")]
async fn specialists_rate_limited_task_posts_survivors_with_note(pool: PgPool) {
    let s = setup(&pool, FakeGit::new(pr("h1"), vec![file("a.rs", PATCH_2)])).await;
    s.chat.push_for(
        Stage::Finder,
        PERFORMANCE_ROLE,
        Reply::Err(ChatError::AllKeysFailed { retry_after: None }),
    );
    s.chat.push_for(
        Stage::Finder,
        BUG_ROLE,
        Reply::text(&finding("a.rs", 2, "Bug")),
    );

    let id = run_mode(&s, Trigger::Opened, Some(ReviewMode::Specialists))
        .await
        .unwrap();

    let row = review(&pool, id).await;
    assert_eq!(row.status, ReviewStatus::Done);
    assert_eq!(row.comments_posted, 1);
    let git = s.git.state();
    let summary = &git.reviews[0].summary;
    assert!(
        summary.contains("**Notes**\n- Performance reviewer did not complete"),
        "{summary}"
    );
    assert!(!summary.contains("Skipped files"), "{summary}");
}

#[sqlx::test(migrations = "../../migrations")]
async fn one_invalid_file_is_skipped_and_review_posted(pool: PgPool) {
    let files = vec![big_file("a.rs"), big_file("b.rs")];
    let s = setup(&pool, FakeGit::new(pr("h1"), files)).await;
    s.chat.push_for(
        Stage::Finder,
        &diff_header("a.rs"),
        Reply::Err(ChatError::Invalid("empty response".into())),
    );
    s.chat.push_for(
        Stage::Finder,
        &diff_header("b.rs"),
        Reply::text(&finding("b.rs", 2, "Bug")),
    );

    let id = run(&s, Trigger::Opened).await.unwrap();

    let row = review(&pool, id).await;
    assert_eq!(row.status, ReviewStatus::Done);
    assert_eq!(row.comments_posted, 1);
    let git = s.git.state();
    assert_eq!(git.reviews[0].comments[0].path, "b.rs");
    assert!(git.reviews[0].summary.contains("`a.rs`: LLM error"));
}

#[sqlx::test(migrations = "../../migrations")]
async fn one_context_overflow_file_is_skipped_and_review_posted(pool: PgPool) {
    let files = vec![big_file("a.rs"), big_file("b.rs")];
    let s = setup(&pool, FakeGit::new(pr("h1"), files)).await;
    let a = diff_header("a.rs");
    // The agent retries an overflow once on a compacted conversation; both fail.
    for _ in 0..2 {
        s.chat.push_for(
            Stage::Finder,
            &a,
            Reply::Err(ChatError::ContextOverflow("prompt is too long".into())),
        );
    }
    s.chat.push_for(
        Stage::Finder,
        &diff_header("b.rs"),
        Reply::text(&finding("b.rs", 2, "Bug")),
    );

    let id = run(&s, Trigger::Opened).await.unwrap();

    let row = review(&pool, id).await;
    assert_eq!(row.status, ReviewStatus::Done);
    assert_eq!(row.comments_posted, 1);
    let finder = s.chat.finder_calls();
    assert_eq!(finder.iter().filter(|c| c.user.contains(&a)).count(), 2);
    let git = s.git.state();
    assert_eq!(git.reviews[0].comments[0].path, "b.rs");
    assert!(git.reviews[0].summary.contains("`a.rs`: LLM error"));
}

#[sqlx::test(migrations = "../../migrations")]
async fn every_file_invalid_is_fatal(pool: PgPool) {
    let files = vec![big_file("a.rs"), big_file("b.rs")];
    let s = setup(&pool, FakeGit::new(pr("h1"), files)).await;
    for path in ["a.rs", "b.rs"] {
        s.chat.push_for(
            Stage::Finder,
            &diff_header(path),
            Reply::Err(ChatError::Invalid("model not found".into())),
        );
    }

    let err = run(&s, Trigger::Opened).await.unwrap_err();

    assert!(
        matches!(&err, EngineError::Fatal(m) if m.contains("model not found")),
        "{err:?}"
    );
    assert_eq!(s.chat.finder_calls().len(), 2);
    assert!(s.git.state().reviews.is_empty());
    let row = latest(&pool).await;
    assert_eq!(row.status, ReviewStatus::Failed);
}

/// R-T8-3: every Finder task unparseable is a clean failure, never an empty "No issues" review.
#[sqlx::test(migrations = "../../migrations")]
async fn every_batch_unparseable_is_fatal(pool: PgPool) {
    let s = setup(&pool, FakeGit::new(pr("h1"), vec![file("a.rs", PATCH_2)])).await;
    s.chat.push(Ok("looks fine"));
    s.chat.push(Ok("still fine"));

    let err = run(&s, Trigger::Opened).await.unwrap_err();

    assert!(
        matches!(&err, EngineError::Fatal(m) if m.contains("could not be parsed")),
        "{err:?}"
    );
    assert!(s.git.state().reviews.is_empty());
    assert_eq!(latest(&pool).await.status, ReviewStatus::Failed);
}

#[sqlx::test(migrations = "../../migrations")]
async fn concurrent_batches_keep_comment_order(pool: PgPool) {
    let paths = ["a.rs", "b.rs", "c.rs", "d.rs", "e.rs", "f.rs"];
    let files = paths.iter().map(|p| big_file(p)).collect();
    let s = setup(&pool, FakeGit::new(pr("h1"), files)).await;
    for path in paths {
        s.chat.push_for(
            Stage::Finder,
            &diff_header(path),
            Reply::text(&finding(path, 2, path)),
        );
    }
    // Yang pertama paling lambat: tanpa urutan terjaga, komentarnya akan terakhir.
    s.chat
        .delay(&diff_header("a.rs"), Duration::from_millis(300));
    s.chat
        .delay(&diff_header("b.rs"), Duration::from_millis(150));

    run(&s, Trigger::Opened).await.unwrap();

    let posted: Vec<String> = s.git.state().reviews[0]
        .comments
        .iter()
        .map(|c| c.path.clone())
        .collect();
    assert_eq!(posted, paths);
    assert_eq!(s.chat.finder_calls().len(), 6, "one batch per big file");
    let finder = s.chat.max_in_flight(Stage::Finder);
    assert!(finder > 1, "batches are reviewed concurrently");
    assert!(finder <= 2, "at most BATCH_CONCURRENCY = 2 batches at once");
    let verifier = s.chat.max_in_flight(Stage::Verifier);
    assert!(verifier > 1, "candidates are verified concurrently");
    assert!(verifier <= 4, "at most 4 verifiers at once");
}

#[sqlx::test(migrations = "../../migrations")]
async fn no_model_configured_fails_with_pr_comment(pool: PgPool) {
    let s = setup(&pool, FakeGit::new(pr("h1"), vec![file("a.rs", PATCH_2)])).await;
    let mut current = settings::get(&pool).await.unwrap();
    current.default_provider_id = None;
    settings::update(&pool, &current).await.unwrap();

    let err = run(&s, Trigger::Opened).await.unwrap_err();

    assert_eq!(
        err,
        EngineError::Fatal("no review model configured".to_string())
    );
    {
        let git = s.git.state();
        assert_eq!(git.comments.len(), 1);
        assert!(git.comments[0].contains(ADUDU_MARKER));
        assert!(git.comments[0].contains("no AI model is configured"));
        assert!(git.reviews.is_empty());
    }
    assert!(s.chat.calls().is_empty());
    let rows = reviews::list(&pool, None, 1, 10).await.unwrap();
    assert_eq!(rows[0].status, ReviewStatus::Failed);
    assert_eq!(rows[0].error.as_deref(), Some("no review model configured"));
}

#[sqlx::test(migrations = "../../migrations")]
async fn no_model_on_updated_is_skipped_not_failed(pool: PgPool) {
    let s = setup(&pool, FakeGit::new(pr("h1"), vec![file("a.rs", PATCH_2)])).await;
    let mut current = settings::get(&pool).await.unwrap();
    current.default_provider_id = None;
    settings::update(&pool, &current).await.unwrap();

    let id = run(&s, Trigger::Updated).await.unwrap();

    let row = review(&pool, id).await;
    assert_eq!(row.status, ReviewStatus::Skipped);
    assert_eq!(row.skip_reason.as_deref(), Some("no_model"));
    assert!(row.error.is_none());
    {
        let git = s.git.state();
        assert!(git.comments.is_empty());
        assert!(git.reviews.is_empty());
    }
    assert!(s.chat.calls().is_empty());

    run(&s, Trigger::Command).await.unwrap_err();
    assert_eq!(s.git.state().comments.len(), 1, "a command still explains");
}

#[sqlx::test(migrations = "../../migrations")]
async fn repo_provider_override_wins(pool: PgPool) {
    let s = setup(&pool, FakeGit::new(pr("h1"), vec![file("a.rs", PATCH_2)])).await;
    let other = adus_db::ai::insert_provider(
        &pool,
        adus_core::types::ProviderKind::Anthropic,
        "a",
        None,
        "claude-test",
        None,
    )
    .await
    .unwrap();
    let patch = RepoPatch {
        review_provider_id: Some(Some(other.id)),
        ..Default::default()
    };
    repositories::update(&pool, s.repository_id, patch)
        .await
        .unwrap();
    s.chat.push(Ok(&finding("a.rs", 2, "Bug")));

    run(&s, Trigger::Opened).await.unwrap();

    let calls = s.chat.calls();
    assert_eq!(calls.len(), 2, "Finder and Verifier");
    assert!(calls.iter().all(|c| c.provider_id == other.id));
    // No fast model: the Verifier falls back to the primary model.
    assert!(calls.iter().all(|c| c.model == "claude-test"));
}

#[sqlx::test(migrations = "../../migrations")]
async fn disabled_provider_treated_as_no_model(pool: PgPool) {
    let s = setup(&pool, FakeGit::new(pr("h1"), vec![file("a.rs", PATCH_2)])).await;
    adus_db::ai::update_provider(
        &pool,
        s.provider_id,
        "openai",
        None,
        Some("gpt-test"),
        Some("gpt-fast"),
        false,
    )
    .await
    .unwrap();

    let id = run(&s, Trigger::Updated).await.unwrap();
    let row = review(&pool, id).await;
    assert_eq!(row.status, ReviewStatus::Skipped);
    assert_eq!(row.skip_reason.as_deref(), Some("no_model"));
    assert!(s.git.state().comments.is_empty());

    let err = run(&s, Trigger::Opened).await.unwrap_err();
    assert_eq!(
        err,
        EngineError::Fatal("no review model configured".to_string())
    );
    assert_eq!(s.git.state().comments.len(), 1);
    assert!(s.chat.calls().is_empty());
}

/// Providers set up before models moved onto providers may have no primary model yet.
#[sqlx::test(migrations = "../../migrations")]
async fn provider_without_primary_model_treated_as_no_model(pool: PgPool) {
    let s = setup(&pool, FakeGit::new(pr("h1"), vec![file("a.rs", PATCH_2)])).await;
    adus_db::ai::update_provider(
        &pool,
        s.provider_id,
        "openai",
        None,
        None,
        Some("gpt-fast"),
        true,
    )
    .await
    .unwrap();

    let id = run(&s, Trigger::Updated).await.unwrap();
    let row = review(&pool, id).await;
    assert_eq!(row.status, ReviewStatus::Skipped);
    assert_eq!(row.skip_reason.as_deref(), Some("no_model"));

    let err = run(&s, Trigger::Opened).await.unwrap_err();
    assert_eq!(
        err,
        EngineError::Fatal("no review model configured".to_string())
    );
    assert!(s.chat.calls().is_empty());
}

#[sqlx::test(migrations = "../../migrations")]
async fn review_mode_adjusts_reasoning_effort(pool: PgPool) {
    let s = setup(&pool, FakeGit::new(pr("h1"), vec![file("a.rs", PATCH_2)])).await;

    // Default (Generalist) -> Medium.
    run(&s, Trigger::Opened).await.unwrap();

    // mode_override = Specialists -> High.
    run_mode(&s, Trigger::Command, Some(ReviewMode::Specialists))
        .await
        .unwrap();

    // repo.review_mode = Specialists -> High.
    repositories::update(
        &pool,
        s.repository_id,
        RepoPatch {
            review_mode: Some(Some(ReviewMode::Specialists)),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    run(&s, Trigger::Opened).await.unwrap();

    // Clear repo override, set settings.review_mode = Specialists -> High.
    repositories::update(
        &pool,
        s.repository_id,
        RepoPatch {
            review_mode: Some(None),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let mut current = settings::get(&pool).await.unwrap();
    current.review_mode = ReviewMode::Specialists;
    settings::update(&pool, &current).await.unwrap();
    run(&s, Trigger::Opened).await.unwrap();

    // Specialists runs Bug, Security and Performance (no rule matches, so no Rules agent).
    let efforts: Vec<Option<ReasoningEffort>> = s
        .chat
        .finder_calls()
        .into_iter()
        .map(|c| c.effort)
        .collect();
    let high = Some(ReasoningEffort::High);
    let mut expected = vec![Some(ReasoningEffort::Medium)];
    expected.extend([high; 9]);
    assert_eq!(efforts, expected);
    assert!(s.chat.finder_calls().iter().all(|c| c.model == "gpt-test"));
}

#[sqlx::test(migrations = "../../migrations")]
async fn review_mode_precedence_command_overrides_repo_overrides_settings(pool: PgPool) {
    let s = setup(&pool, FakeGit::new(pr("h1"), vec![file("a.rs", PATCH_2)])).await;
    let mut current = settings::get(&pool).await.unwrap();
    current.review_mode = ReviewMode::Specialists;
    settings::update(&pool, &current).await.unwrap();
    let set_repo_mode = |mode: Option<ReviewMode>| {
        let pool = pool.clone();
        let id = s.repository_id;
        async move {
            let patch = RepoPatch {
                review_mode: Some(mode),
                ..Default::default()
            };
            repositories::update(&pool, id, patch).await.unwrap();
        }
    };
    // Role of each Finder call made from `from` on.
    let roles_since = |from: usize| -> Vec<&'static str> {
        s.chat.finder_calls()[from..]
            .iter()
            .map(|c| {
                [
                    GENERALIST_ROLE,
                    BUG_ROLE,
                    SECURITY_ROLE,
                    PERFORMANCE_ROLE,
                    RULES_ROLE,
                ]
                .into_iter()
                .find(|role| c.system.contains(role))
                .expect("a Finder role")
            })
            .collect()
    };
    let specialists = vec![BUG_ROLE, SECURITY_ROLE, PERFORMANCE_ROLE];

    // Repo (Generalist) beats settings (Specialists).
    set_repo_mode(Some(ReviewMode::Generalist)).await;
    run(&s, Trigger::Opened).await.unwrap();
    assert_eq!(roles_since(0), vec![GENERALIST_ROLE]);

    // Command (Specialists) beats repo (Generalist).
    run_mode(&s, Trigger::Command, Some(ReviewMode::Specialists))
        .await
        .unwrap();
    assert_eq!(roles_since(1), specialists);

    // No command, no repo override: settings (Specialists).
    set_repo_mode(None).await;
    run(&s, Trigger::Opened).await.unwrap();
    assert_eq!(roles_since(4), specialists);

    // Command (Generalist) beats repo (Specialists).
    set_repo_mode(Some(ReviewMode::Specialists)).await;
    run_mode(&s, Trigger::Command, Some(ReviewMode::Generalist))
        .await
        .unwrap();
    assert_eq!(roles_since(7), vec![GENERALIST_ROLE]);

    let efforts: Vec<_> = s.chat.finder_calls().iter().map(|c| c.effort).collect();
    let (medium, high) = (Some(ReasoningEffort::Medium), Some(ReasoningEffort::High));
    let mut expected = vec![medium];
    expected.extend([high; 6]);
    expected.push(medium);
    assert_eq!(efforts, expected);
}

#[sqlx::test(migrations = "../../migrations")]
async fn unauthorized_pat_marks_connection_invalid(pool: PgPool) {
    let s = setup(&pool, FakeGit::new(pr("h1"), vec![file("a.rs", PATCH_2)])).await;
    s.git.state().get_pr_error = Some(GitError::Unauthorized);

    let err = run(&s, Trigger::Opened).await.unwrap_err();

    assert!(matches!(err, EngineError::Fatal(_)));
    let conn = connections::get(&pool, s.connection_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(conn.status, ConnectionStatus::Invalid);
    let rows = reviews::list(&pool, None, 1, 10).await.unwrap();
    assert_eq!(rows[0].status, ReviewStatus::Failed);

    s.git.state().get_pr_error = Some(GitError::NotFound);
    assert_eq!(
        run(&s, Trigger::Opened).await,
        Err(EngineError::Fatal("pull request not found".to_string()))
    );
    s.git.state().get_pr_error = Some(GitError::RateLimited);
    assert!(matches!(
        run(&s, Trigger::Opened).await,
        Err(EngineError::Retryable(_, None))
    ));
}

#[sqlx::test(migrations = "../../migrations")]
async fn post_error_retryable_on_github_fatal_on_azure(pool: PgPool) {
    let s = setup(&pool, FakeGit::new(pr("h1"), vec![file("a.rs", PATCH_2)])).await;
    for retryable in [
        GitError::Status(502),
        GitError::RateLimited,
        GitError::Http("connection reset".into()),
    ] {
        s.git.state().post_error = Some(retryable);
        assert!(matches!(
            run(&s, Trigger::Opened).await,
            Err(EngineError::Retryable(_, None))
        ));
    }
    assert!(reviews::list(&pool, None, 1, 10).await.unwrap().is_empty());

    // 4xx lain tidak akan berubah dengan mengulang.
    s.git.state().post_error = Some(GitError::Status(422));
    assert!(matches!(
        run(&s, Trigger::Opened).await,
        Err(EngineError::Fatal(_))
    ));
    let row = latest(&pool).await;
    assert_eq!(row.status, ReviewStatus::Failed);
    assert!(row.error.unwrap().contains("posting the review failed"));

    s.git.state().post_error = Some(GitError::Status(502));
    sqlx::query("UPDATE connections SET kind = 'azure_devops'")
        .execute(&pool)
        .await
        .unwrap();
    assert!(matches!(
        run(&s, Trigger::Opened).await,
        Err(EngineError::Fatal(_))
    ));
}

#[sqlx::test(migrations = "../../migrations")]
async fn usage_context_carries_review_id(pool: PgPool) {
    let s = setup(&pool, FakeGit::new(pr("h1"), vec![file("a.rs", PATCH_2)])).await;
    s.chat.push(Ok(&finding("a.rs", 2, "Bug")));

    let id = run(&s, Trigger::Opened).await.unwrap();

    let calls = s.chat.calls();
    assert_eq!(calls.len(), 2, "Finder and Verifier");
    for call in calls {
        assert_eq!(call.ctx.review_id, Some(id));
        assert_eq!(call.ctx.repository_id, Some(s.repository_id));
    }
}

#[sqlx::test(migrations = "../../migrations")]
async fn no_model_comment_unauthorized_marks_connection_invalid(pool: PgPool) {
    let s = setup(&pool, FakeGit::new(pr("h1"), vec![file("a.rs", PATCH_2)])).await;
    let mut current = settings::get(&pool).await.unwrap();
    current.default_provider_id = None;
    settings::update(&pool, &current).await.unwrap();
    s.git.state().comment_error = Some(GitError::Unauthorized);

    let err = run(&s, Trigger::Opened).await.unwrap_err();

    assert_eq!(
        err,
        EngineError::Fatal("no review model configured".to_string())
    );
    let conn = connections::get(&pool, s.connection_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(conn.status, ConnectionStatus::Invalid);
}

#[sqlx::test(migrations = "../../migrations")]
async fn missing_previous_head_falls_back_to_full_review(pool: PgPool) {
    let s = setup(&pool, FakeGit::new(pr("h1"), vec![file("a.rs", PATCH_2)])).await;
    run(&s, Trigger::Opened).await.unwrap();
    {
        let mut git = s.git.state();
        git.pr = pr("h2"); // force-push: h1 sudah tidak ada di provider
        git.full = vec![file("a.rs", PATCH_234)];
        git.diff_between_error = Some(GitError::NotFound);
    }
    s.chat.push(Ok(&finding("a.rs", 2, "Full again")));

    let id = run(&s, Trigger::Updated).await.unwrap();

    assert_eq!(review(&pool, id).await.status, ReviewStatus::Done);
    let git = s.git.state();
    assert_eq!(git.diff_between_calls.len(), 1);
    let comments = &git.reviews[1].comments;
    assert_eq!(comments.len(), 1);
    assert_eq!(
        comments[0].line, 2,
        "line 2 is only commentable in the full diff"
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn fetch_error_after_get_pr_is_retryable_and_leaves_no_row(pool: PgPool) {
    let s = setup(&pool, FakeGit::new(pr("h1"), vec![file("a.rs", PATCH_2)])).await;
    s.git.state().pr_diff_error = Some(GitError::Http("503".into()));

    let err = run(&s, Trigger::Opened).await.unwrap_err();

    assert!(matches!(err, EngineError::Retryable(_, None)));
    assert!(reviews::list(&pool, None, 1, 10).await.unwrap().is_empty());
    assert!(s.git.state().reviews.is_empty());
}

#[sqlx::test(migrations = "../../migrations")]
async fn nothing_to_review_posts_nothing(pool: PgPool) {
    let s = setup(&pool, FakeGit::new(pr("h1"), vec![file("a.rs", PATCH_2)])).await;
    run(&s, Trigger::Opened).await.unwrap();
    {
        let mut git = s.git.state();
        git.pr = pr("h2");
        // Commit baru hanya menghapus baris.
        git.incremental = vec![file(
            "a.rs",
            "@@ -1,2 +1,1 @@
 a
-b
",
        )];
    }

    let id = run(&s, Trigger::Updated).await.unwrap();

    let row = review(&pool, id).await;
    assert_eq!(row.status, ReviewStatus::Skipped);
    assert_eq!(row.skip_reason.as_deref(), Some("nothing_to_review"));
    assert_eq!(s.git.state().reviews.len(), 1, "only the first review");
    assert_eq!(s.chat.calls().len(), 1);
}

/// Once the mode is known, the fetches that follow are bounded by that mode's budget, not
/// only by the larger backstop of `review_pr`.
#[sqlx::test(migrations = "../../migrations")]
async fn generalist_fetch_phase_is_bounded_by_its_own_budget(pool: PgPool) {
    let mut s = setup(
        &pool,
        FakeGit::new(pr("h1"), vec![file("src/a.rs", PATCH_234)]),
    )
    .await;
    s.git.state().pr_diff_delay = Duration::from_secs(30);
    s.engine.review_timeout = Duration::from_millis(300);
    let started = Instant::now();

    let err = run(&s, Trigger::Opened).await.unwrap_err();

    assert_eq!(err, EngineError::Fatal("review timed out".into()));
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "cut at the Generalist budget"
    );
    let row = latest(&pool).await;
    assert_eq!(row.status, ReviewStatus::Failed);
    assert_eq!(row.error.as_deref(), Some("review timed out"));
}

#[sqlx::test(migrations = "../../migrations")]
async fn slow_review_times_out_and_is_marked_failed(pool: PgPool) {
    let mut s = setup(
        &pool,
        FakeGit::new(pr("h1"), vec![file("src/a.rs", PATCH_234)]),
    )
    .await;
    s.git.state().get_pr_delay = Duration::from_secs(5);
    s.engine.review_timeout = Duration::from_millis(100);
    s.engine.specialists_review_timeout = Duration::from_millis(100);

    let err = run(&s, Trigger::Opened).await.unwrap_err();

    assert_eq!(err, EngineError::Fatal("review timed out".into()));
    let row = latest(&pool).await;
    assert_eq!(row.status, ReviewStatus::Failed);
    assert_eq!(row.error.as_deref(), Some("review timed out"));
    assert!(row.finished_at.is_some());
}

/// Simpan daftar ignore (`None` = bawaan) dan kata kunci judul.
async fn set_ignore(pool: &PgPool, paths: Option<&[&str]>, keywords: &[&str]) {
    let mut current = settings::get(pool).await.unwrap();
    current.ignore_paths = paths.map(|p| p.iter().map(|s| s.to_string()).collect());
    current.ignored_title_keywords = keywords.iter().map(|s| s.to_string()).collect();
    settings::update(pool, &current).await.unwrap();
}

#[sqlx::test(migrations = "../../migrations")]
async fn ignored_files_not_sent_to_llm(pool: PgPool) {
    // File terhapus yang cocok dengan daftar ignore tidak ikut dihitung.
    let mut deleted = file(
        "gone.json",
        "@@ -1,1 +0,0 @@
-a
",
    );
    deleted.status = FileStatus::Deleted;
    let files = vec![file("a.json", PATCH_2), deleted, file("b.rs", PATCH_2)];
    let s = setup(&pool, FakeGit::new(pr("h1"), files)).await;
    set_ignore(&pool, Some(&["**/*.json"]), &[]).await;

    let id = run(&s, Trigger::Opened).await.unwrap();

    assert_eq!(review(&pool, id).await.status, ReviewStatus::Done);
    let calls = s.chat.finder_calls();
    assert_eq!(calls.len(), 1);
    assert!(calls[0].user.contains(&diff_header("b.rs")));
    assert!(!calls[0].user.contains(&diff_header("a.json")));
    let summary = s.git.state().reviews[0].summary.clone();
    assert!(summary.contains("1 file ignored"));
    assert!(!summary.contains("a.json"), "ignored files are not listed");
}

#[sqlx::test(migrations = "../../migrations")]
async fn ignored_files_do_not_count_toward_file_limit(pool: PgPool) {
    let files = vec![file("a.json", PATCH_2), file("b.rs", PATCH_2)];
    let s = setup(&pool, FakeGit::new(pr("h1"), files)).await;
    set_ignore(&pool, Some(&["**/*.json"]), &[]).await;
    set_limits(&pool, 1, 2000).await;

    run(&s, Trigger::Opened).await.unwrap();

    let calls = s.chat.finder_calls();
    assert_eq!(calls.len(), 1);
    assert!(calls[0].user.contains(&diff_header("b.rs")));
    let summary = s.git.state().reviews[0].summary.clone();
    assert!(!summary.contains("file limit reached"));
}

#[sqlx::test(migrations = "../../migrations")]
async fn all_files_ignored_is_skipped_nothing_to_review(pool: PgPool) {
    let files = vec![file("a.json", PATCH_2), file("dist/b.js", PATCH_2)];
    let s = setup(&pool, FakeGit::new(pr("h1"), files)).await;
    set_ignore(&pool, Some(&["**/*.json", "dist/**"]), &[]).await;

    let id = run(&s, Trigger::Opened).await.unwrap();

    let row = review(&pool, id).await;
    assert_eq!(row.status, ReviewStatus::Skipped);
    assert_eq!(row.skip_reason.as_deref(), Some("nothing_to_review"));
    assert!(s.chat.calls().is_empty());
    let git = s.git.state();
    assert!(git.reviews.is_empty());
    assert!(git.comments.is_empty());
}

#[sqlx::test(migrations = "../../migrations")]
async fn incremental_review_applies_ignore(pool: PgPool) {
    let s = setup(&pool, FakeGit::new(pr("h1"), vec![file("a.rs", PATCH_2)])).await;
    run(&s, Trigger::Opened).await.unwrap();
    set_ignore(&pool, Some(&["**/*.json"]), &[]).await;
    {
        let mut git = s.git.state();
        git.pr = pr("h2");
        git.full = vec![file("a.rs", PATCH_234), file("b.json", PATCH_2)];
        git.incremental = vec![
            file("a.rs", "@@ -3,1 +3,2 @@\n c\n+d\n"),
            file("b.json", PATCH_2),
        ];
    }

    let id = run(&s, Trigger::Updated).await.unwrap();

    assert_eq!(review(&pool, id).await.status, ReviewStatus::Done);
    let git = s.git.state();
    assert_eq!(git.diff_between_calls.len(), 1, "incremental path taken");
    let calls = s.chat.finder_calls();
    assert_eq!(calls.len(), 2, "first run a.rs, incremental run a.rs only");
    assert!(calls[1].user.contains(&diff_header("a.rs")));
    assert!(!calls[1].user.contains(&diff_header("b.json")));
    assert!(git.reviews[1].summary.contains("1 file ignored"));
}

#[sqlx::test(migrations = "../../migrations")]
async fn ignored_title_skips_auto_but_not_command(pool: PgPool) {
    let mut wip = pr("h1");
    wip.title = "WIP: Add login".into();
    let s = setup(&pool, FakeGit::new(wip, vec![file("a.rs", PATCH_2)])).await;
    set_ignore(&pool, None, &["wip"]).await;
    // Diff tidak boleh diambil untuk review yang dilewati.
    s.git.state().pr_diff_error = Some(GitError::Http("503".into()));

    for trigger in [Trigger::Opened, Trigger::Updated] {
        let id = run(&s, trigger).await.unwrap();
        let row = review(&pool, id).await;
        assert_eq!(row.status, ReviewStatus::Skipped);
        assert_eq!(row.skip_reason.as_deref(), Some("ignored_title"));
    }
    assert!(s.chat.calls().is_empty());
    assert!(s.git.state().reviews.is_empty());
    assert!(s.git.state().comments.is_empty());

    s.git.state().pr_diff_error = None;
    let id = run(&s, Trigger::Command).await.unwrap();
    assert_eq!(review(&pool, id).await.status, ReviewStatus::Done);
    assert_eq!(s.git.state().reviews.len(), 1);
}

#[sqlx::test(migrations = "../../migrations")]
async fn invalid_stored_ignore_list_falls_back_to_defaults(pool: PgPool) {
    let files = vec![file("yarn.lock", PATCH_2), file("a.rs", PATCH_2)];
    let s = setup(&pool, FakeGit::new(pr("h1"), files)).await;
    set_ignore(&pool, Some(&["a[b"]), &[]).await;

    let id = run(&s, Trigger::Opened).await.unwrap();

    assert_eq!(review(&pool, id).await.status, ReviewStatus::Done);
    let calls = s.chat.finder_calls();
    assert_eq!(calls.len(), 1, "one batch");
    assert!(calls[0].user.contains(&diff_header("a.rs")));
    assert!(
        !calls[0].user.contains(&diff_header("yarn.lock")),
        "yarn.lock ignored by the defaults"
    );
}

// ---- Agentic pipeline: clone, tools, specialists, Verifier, Dedup, deadlines ----

#[sqlx::test(migrations = "../../migrations")]
async fn agentic_generalist_uses_tools_verifies_and_dedups(pool: PgPool) {
    let s = setup(
        &pool,
        FakeGit::new(pr("h1"), vec![file("src/login.rs", PATCH_234)]),
    )
    .await;
    s.git.state().clone_spec = Some(clone_spec());
    s.cloner.files(&[
        ("src/login.rs", "a\nb\nc\nd\n"),
        (
            "src/helper.rs",
            "pub fn is_admin() -> bool {\n    false\n}\n",
        ),
    ]);
    s.chat.push_stage(
        Stage::Finder,
        Reply::tool("read_file", json!({"path": "src/helper.rs"})),
    );
    s.chat.push_stage(
        Stage::Finder,
        Reply::submit(json!([
            finding_json("src/login.rs", 2, "critical", "Real bug"),
            finding_json("src/login.rs", 3, "warning", "False alarm"),
        ])),
    );
    s.chat.push_for(
        Stage::Verifier,
        "False alarm",
        Reply::text(r#"{"keep": false, "rationale": "is_admin guards it"}"#),
    );

    let id = run(&s, Trigger::Opened).await.unwrap();

    let row = review(&pool, id).await;
    assert_eq!(row.status, ReviewStatus::Done);
    assert_eq!(row.comments_posted, 1);

    // Finder: primary model at Medium, repository tools, then the helper file's content.
    let finder = s.chat.finder_calls();
    assert_eq!(finder.len(), 2);
    for call in &finder {
        assert_eq!(call.model, "gpt-test");
        assert_eq!(call.effort, Some(ReasoningEffort::Medium));
        assert!(call.system.contains(GENERALIST_ROLE));
        assert!(call.system.contains("You can read the repository"));
    }
    assert_eq!(
        finder[0].tools,
        vec![
            "read_file",
            "grep",
            "list_dir",
            "find_files",
            "submit_findings"
        ]
    );
    assert!(finder[0].tool_results.is_empty());
    assert_eq!(finder[1].tool_results.len(), 1);
    assert!(finder[1].tool_results[0].contains("pub fn is_admin() -> bool"));

    // Verifier: fast model at Low, with tools, one call per candidate.
    let verifier = s.chat.verifier_calls();
    assert_eq!(verifier.len(), 2);
    for call in &verifier {
        assert_eq!(call.model, "gpt-fast");
        assert_eq!(call.effort, Some(ReasoningEffort::Low));
        assert!(call.tools.contains(&"read_file".to_string()));
        assert!(call.tools.contains(&"submit_verdict".to_string()));
    }
    // One verified finding is left: semantic dedup needs at least two.
    assert!(s.chat.dedup_calls().is_empty());

    let git = s.git.state();
    let posted = &git.reviews[0];
    assert_eq!(posted.comments.len(), 1);
    assert_eq!(posted.comments[0].line, 2);
    assert!(posted.comments[0].body.contains("Real bug"));
    assert!(posted.summary.contains("Critical: 1"));
    assert!(!posted.summary.contains("Warning"));
    assert!(!posted.summary.contains("**Notes**"));

    assert_eq!(s.cloner.urls(), vec![clone_spec().url]);
    let root = &s.cloner.roots()[0];
    assert!(!root.exists(), "the workspace is deleted after the review");
}

#[sqlx::test(migrations = "../../migrations")]
async fn specialists_mode_runs_bug_security_performance_and_rules_and_dedups(pool: PgPool) {
    let s = setup(
        &pool,
        FakeGit::new(pr("h1"), vec![file("src/a.rs", PATCH_234)]),
    )
    .await;
    insert_rule(&pool).await;
    s.chat.push_for(
        Stage::Finder,
        BUG_ROLE,
        Reply::text(
            &json!([finding_json(
                "src/a.rs",
                2,
                "critical",
                "Missing null check"
            )])
            .to_string(),
        ),
    );
    s.chat.push_for(
        Stage::Finder,
        SECURITY_ROLE,
        Reply::text(
            &json!([finding_json("src/a.rs", 2, "critical", "Unvalidated input")]).to_string(),
        ),
    );
    s.chat
        .push_stage(Stage::Dedup, Reply::text(r#"{"keep_indices": [0]}"#));

    let id = run_mode(&s, Trigger::Command, Some(ReviewMode::Specialists))
        .await
        .unwrap();

    let row = review(&pool, id).await;
    assert_eq!(row.status, ReviewStatus::Done);
    assert_eq!(row.comments_posted, 1);

    let finder = s.chat.finder_calls();
    assert_eq!(finder.len(), 4, "Bug, Security, Performance and Rules");
    for role in [BUG_ROLE, SECURITY_ROLE, PERFORMANCE_ROLE, RULES_ROLE] {
        assert_eq!(
            finder.iter().filter(|c| c.system.contains(role)).count(),
            1,
            "{role}"
        );
    }
    for call in &finder {
        assert_eq!(call.model, "gpt-test");
        assert_eq!(call.effort, Some(ReasoningEffort::High));
        assert!(call.user.contains("No unwrap"));
    }

    // The Verifier keeps both findings; dedup merges them into one.
    let verifier = s.chat.verifier_calls();
    assert_eq!(verifier.len(), 2);
    assert!(verifier.iter().all(|c| c.model == "gpt-fast"));
    let dedup = s.chat.dedup_calls();
    assert_eq!(dedup.len(), 1);
    assert_eq!(dedup[0].model, "gpt-fast");
    assert_eq!(dedup[0].effort, Some(ReasoningEffort::Low));
    assert!(dedup[0].user.contains("Missing null check"));
    assert!(dedup[0].user.contains("Unvalidated input"));
    assert!(dedup[0].user.contains("Bug reviewer"));
    assert!(dedup[0].user.contains("Security reviewer"));

    let git = s.git.state();
    let posted = &git.reviews[0];
    assert_eq!(posted.comments.len(), 1);
    assert!(posted.comments[0].body.contains("Missing null check"));
    assert!(posted.summary.contains("Critical: 1"));
}

#[sqlx::test(migrations = "../../migrations")]
async fn partial_specialist_failure_publishes_surviving_with_warning(pool: PgPool) {
    let s = setup(&pool, FakeGit::new(pr("h1"), vec![file("a.rs", PATCH_2)])).await;
    s.chat.push_for(
        Stage::Finder,
        PERFORMANCE_ROLE,
        Reply::Err(ChatError::Invalid("bad request".into())),
    );
    s.chat.push_for(
        Stage::Finder,
        BUG_ROLE,
        Reply::text(&finding("a.rs", 2, "Bug")),
    );

    let id = run_mode(&s, Trigger::Opened, Some(ReviewMode::Specialists))
        .await
        .unwrap();

    let row = review(&pool, id).await;
    assert_eq!(row.status, ReviewStatus::Done);
    assert_eq!(row.comments_posted, 1);
    let git = s.git.state();
    let posted = &git.reviews[0];
    assert!(posted.comments[0].body.contains("Bug"));
    assert!(
        posted
            .summary
            .contains("**Notes**\n- Performance reviewer did not complete"),
        "{}",
        posted.summary
    );
    assert!(!posted.summary.contains("Skipped files"));
}

#[sqlx::test(migrations = "../../migrations")]
async fn clone_failure_falls_back_to_diff_only_with_summary_note(pool: PgPool) {
    let s = setup(&pool, FakeGit::new(pr("h1"), vec![file("a.rs", PATCH_2)])).await;
    s.git.state().clone_spec = Some(clone_spec());
    let failures = [
        WorkspaceError::TimedOut,
        WorkspaceError::TooLarge,
        WorkspaceError::Unavailable("network".into()),
    ];
    for (index, failure) in failures.into_iter().enumerate() {
        s.cloner.set(CloneBehavior::Fail(failure));
        s.chat.push(Ok(&finding("a.rs", 2, "Bug")));

        let id = run(&s, Trigger::Opened).await.unwrap();

        assert_eq!(review(&pool, id).await.status, ReviewStatus::Done);
        let finder = s.chat.finder_calls();
        let call = finder.last().unwrap();
        assert_eq!(call.tools, vec!["submit_findings"], "no repository tools");
        assert!(call.system.contains("You cannot open other files"));
        let verifier = s.chat.verifier_calls();
        assert!(
            verifier.last().unwrap().tools.is_empty(),
            "diff-only Verifier"
        );
        let git = s.git.state();
        let posted = &git.reviews[index];
        assert_eq!(posted.comments.len(), 1);
        assert!(
            posted
                .summary
                .contains(&format!("**Notes**\n- {CODEBASE_NOTE}")),
            "{}",
            posted.summary
        );
    }
    assert_eq!(s.cloner.urls().len(), 3);
    let conn = connections::get(&pool, s.connection_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(conn.status, ConnectionStatus::Ok);
}

const SMALL_PR_NOTE: &str = "Small change — reviewed diff only";

/// Turn the small-PR path on (the shared `setup` disables it).
async fn enable_small_pr(pool: &PgPool) {
    let mut current = settings::get(pool).await.unwrap();
    current.small_pr_max_lines = 50;
    settings::update(pool, &current).await.unwrap();
}

#[sqlx::test(migrations = "../../migrations")]
async fn small_pr_skips_clone_and_tools_but_still_verifies(pool: PgPool) {
    let s = setup(&pool, FakeGit::new(pr("h1"), vec![file("a.rs", PATCH_2)])).await;
    enable_small_pr(&pool).await;
    s.git.state().clone_spec = Some(clone_spec());
    s.chat.push(Ok(&finding("a.rs", 2, "Bug")));

    let id = run(&s, Trigger::Opened).await.unwrap();

    assert_eq!(review(&pool, id).await.status, ReviewStatus::Done);
    assert!(s.cloner.urls().is_empty(), "no clone for a small PR");
    let finder = s.chat.finder_calls();
    assert_eq!(finder.len(), 1);
    assert_eq!(
        finder[0].tools,
        vec!["submit_findings"],
        "no repository tools"
    );
    assert!(finder[0].system.contains("You cannot open other files"));
    let verifier = s.chat.verifier_calls();
    assert_eq!(verifier.len(), 1, "the Verifier still runs");
    assert!(verifier[0].tools.is_empty(), "diff-only Verifier");
    let git = s.git.state();
    assert_eq!(git.reviews[0].comments.len(), 1);
    let summary = &git.reviews[0].summary;
    assert!(
        summary.contains(&format!(
            "**Notes**
- {SMALL_PR_NOTE}"
        )),
        "{summary}"
    );
    assert!(!summary.contains(CODEBASE_NOTE), "not a fallback failure");
}

#[sqlx::test(migrations = "../../migrations")]
async fn command_review_of_small_pr_runs_full_review(pool: PgPool) {
    let s = setup(&pool, FakeGit::new(pr("h1"), vec![file("a.rs", PATCH_2)])).await;
    enable_small_pr(&pool).await;
    s.git.state().clone_spec = Some(clone_spec());
    s.cloner.files(&[(
        "a.rs", "a
b
",
    )]);

    let id = run(&s, Trigger::Command).await.unwrap();

    assert_eq!(review(&pool, id).await.status, ReviewStatus::Done);
    assert_eq!(s.cloner.urls().len(), 1, "@adudu review clones");
    let finder = s.chat.finder_calls();
    assert!(finder[0].tools.contains(&"read_file".to_string()));
    assert!(!s.git.state().reviews[0].summary.contains(SMALL_PR_NOTE));

    // `@adudu review deep` runs the specialists, also on a small PR.
    let before = s.chat.finder_calls().len();
    run_mode(&s, Trigger::Command, Some(ReviewMode::Specialists))
        .await
        .unwrap();
    assert!(s.chat.finder_calls().len() - before > 1);
    assert_eq!(s.cloner.urls().len(), 2);
}

#[sqlx::test(migrations = "../../migrations")]
async fn small_pr_in_specialists_mode_runs_generalist_once(pool: PgPool) {
    let s = setup(&pool, FakeGit::new(pr("h1"), vec![file("a.rs", PATCH_2)])).await;
    enable_small_pr(&pool).await;
    let mut current = settings::get(&pool).await.unwrap();
    current.review_mode = ReviewMode::Specialists;
    settings::update(&pool, &current).await.unwrap();
    s.chat.push(Ok(&finding("a.rs", 2, "Bug")));

    run(&s, Trigger::Opened).await.unwrap();

    let finder = s.chat.finder_calls();
    assert_eq!(finder.len(), 1, "one Generalist pass, no specialists");
    assert!(finder[0].system.contains(GENERALIST_ROLE));
    assert_eq!(finder[0].effort, Some(ReasoningEffort::Medium));
    assert_eq!(s.chat.verifier_calls().len(), 1);
}

#[sqlx::test(migrations = "../../migrations")]
async fn risky_small_pr_gets_full_review(pool: PgPool) {
    let s = setup(
        &pool,
        FakeGit::new(pr("h1"), vec![file("src/auth/login.rs", PATCH_2)]),
    )
    .await;
    enable_small_pr(&pool).await;
    s.git.state().clone_spec = Some(clone_spec());
    s.cloner.files(&[(
        "src/auth/login.rs",
        "a
b
",
    )]);

    let id = run(&s, Trigger::Opened).await.unwrap();

    assert_eq!(review(&pool, id).await.status, ReviewStatus::Done);
    assert_eq!(s.cloner.urls().len(), 1, "risky path is cloned");
    assert!(
        s.chat.finder_calls()[0]
            .tools
            .contains(&"read_file".to_string())
    );
    assert!(!s.git.state().reviews[0].summary.contains(SMALL_PR_NOTE));
}

#[sqlx::test(migrations = "../../migrations")]
async fn clone_unauthorized_marks_connection_invalid_and_fails(pool: PgPool) {
    let s = setup(&pool, FakeGit::new(pr("h1"), vec![file("a.rs", PATCH_2)])).await;
    s.git.state().clone_spec = Some(clone_spec());
    s.cloner
        .set(CloneBehavior::Fail(WorkspaceError::Unauthorized));

    let err = run(&s, Trigger::Opened).await.unwrap_err();

    assert_eq!(
        err,
        EngineError::Fatal("the git provider rejected the token of connection \"gh\"".into())
    );
    let conn = connections::get(&pool, s.connection_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(conn.status, ConnectionStatus::Invalid);
    assert!(s.chat.calls().is_empty());
    assert!(s.git.state().reviews.is_empty());
    assert_eq!(latest(&pool).await.status, ReviewStatus::Failed);
}

#[sqlx::test(migrations = "../../migrations")]
async fn tools_unsupported_falls_back_to_diff_only_with_summary_note(pool: PgPool) {
    let s = setup(&pool, FakeGit::new(pr("h1"), vec![file("a.rs", PATCH_2)])).await;
    s.git.state().clone_spec = Some(clone_spec());
    s.cloner.files(&[("a.rs", "a\nb\n")]);
    s.chat.push(Err(ChatError::ToolsUnsupported(
        "model does not support tools".into(),
    )));
    s.chat.push(Ok(&finding("a.rs", 2, "Bug")));

    let id = run(&s, Trigger::Opened).await.unwrap();

    let row = review(&pool, id).await;
    assert_eq!(row.status, ReviewStatus::Done);
    assert_eq!(row.comments_posted, 1);
    let finder = s.chat.finder_calls();
    assert_eq!(finder.len(), 2, "tool attempt, then the diff-only re-run");
    assert!(finder[0].tools.contains(&"read_file".to_string()));
    assert!(finder[1].tools.is_empty(), "no tools at all on the re-run");
    assert!(finder[1].system.contains("You cannot open other files"));
    let verifier = s.chat.verifier_calls();
    assert_eq!(verifier.len(), 1);
    assert!(verifier[0].tools.is_empty(), "diff-only Verifier");
    let git = s.git.state();
    let summary = &git.reviews[0].summary;
    assert!(
        summary.contains(&format!("**Notes**\n- {TOOLS_NOTE}")),
        "{summary}"
    );
    assert_eq!(summary.matches(TOOLS_NOTE).count(), 1);
    assert!(!summary.contains(CODEBASE_NOTE));
    assert!(!s.cloner.roots()[0].exists());
}

#[sqlx::test(migrations = "../../migrations")]
async fn verifier_fail_closed_drops_unverified_finding_with_warning(pool: PgPool) {
    let s = setup(&pool, FakeGit::new(pr("h1"), vec![file("a.rs", PATCH_234)])).await;
    s.chat.push(Ok(&json!([
        finding_json("a.rs", 2, "warning", "Kept one"),
        finding_json("a.rs", 3, "warning", "Lost one"),
    ])
    .to_string()));
    s.chat.push_for(
        Stage::Verifier,
        "Lost one",
        Reply::Err(ChatError::Invalid("bad request".into())),
    );

    let id = run(&s, Trigger::Opened).await.unwrap();

    let row = review(&pool, id).await;
    assert_eq!(row.status, ReviewStatus::Done);
    assert_eq!(row.comments_posted, 1);
    assert_eq!(s.chat.verifier_calls().len(), 2);
    let git = s.git.state();
    let posted = &git.reviews[0];
    assert!(posted.comments[0].body.contains("Kept one"));
    assert!(
        posted
            .summary
            .contains("**Notes**\n- 1 candidate finding(s) could not be verified and were omitted"),
        "{}",
        posted.summary
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn verifier_all_retryable_failures_retry_the_review(pool: PgPool) {
    let s = setup(&pool, FakeGit::new(pr("h1"), vec![file("a.rs", PATCH_2)])).await;
    s.chat.push(Ok(&finding("a.rs", 2, "Bug")));
    s.chat.push_stage(
        Stage::Verifier,
        Reply::Err(ChatError::AllKeysFailed {
            retry_after: Some(Duration::from_secs(20)),
        }),
    );

    let err = run(&s, Trigger::Opened).await.unwrap_err();

    assert!(
        matches!(err, EngineError::Retryable(_, Some(after)) if after == Duration::from_secs(20)),
        "{err:?}"
    );
    assert!(
        s.git.state().reviews.is_empty(),
        "never posted as no issues"
    );
    assert!(reviews::list(&pool, None, 1, 10).await.unwrap().is_empty());
}

#[sqlx::test(migrations = "../../migrations")]
async fn repo_instructions_loaded_from_clone_into_prompt(pool: PgPool) {
    let s = setup(&pool, FakeGit::new(pr("h1"), vec![file("a.rs", PATCH_2)])).await;
    s.git.state().clone_spec = Some(clone_spec());
    s.cloner.files(&[
        ("a.rs", "a\nb\n"),
        ("AGENTS.md", "Always log with tracing, never println."),
    ]);
    s.chat.push(Ok(&finding("a.rs", 2, "Bug")));

    run(&s, Trigger::Opened).await.unwrap();

    let finder = s.chat.finder_calls();
    assert_eq!(finder.len(), 1);
    assert!(finder[0].user.contains("<repository_instructions>"));
    assert!(
        finder[0]
            .user
            .contains("--- AGENTS.md ---\nAlways log with tracing, never println.")
    );
    let verifier = s.chat.verifier_calls();
    assert_eq!(verifier.len(), 1);
    assert!(
        verifier[0]
            .user
            .contains("Always log with tracing, never println.")
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn finder_deadline_cuts_slow_specialist_and_posts_survivors_with_note(pool: PgPool) {
    let mut s = setup(&pool, FakeGit::new(pr("h1"), vec![file("a.rs", PATCH_2)])).await;
    // Finder deadline at 60 % = 1.8 s; the Performance reviewer would answer after 30 s.
    s.engine.specialists_review_timeout = Duration::from_secs(3);
    s.chat.delay(PERFORMANCE_ROLE, Duration::from_secs(30));
    s.chat.push_for(
        Stage::Finder,
        BUG_ROLE,
        Reply::text(&finding("a.rs", 2, "Bug")),
    );
    let started = Instant::now();

    let id = run_mode(&s, Trigger::Opened, Some(ReviewMode::Specialists))
        .await
        .unwrap();

    assert!(started.elapsed() < Duration::from_secs(3));
    let row = review(&pool, id).await;
    assert_eq!(row.status, ReviewStatus::Done);
    assert_eq!(row.comments_posted, 1);
    let git = s.git.state();
    let summary = &git.reviews[0].summary;
    assert!(
        summary.contains("**Notes**\n- Performance reviewer did not finish in time"),
        "{summary}"
    );
    assert!(!summary.contains("Bug reviewer"));
    assert!(!summary.contains("Security reviewer"));
}

#[sqlx::test(migrations = "../../migrations")]
async fn finder_deadline_with_no_finished_task_fails_timed_out(pool: PgPool) {
    let mut s = setup(&pool, FakeGit::new(pr("h1"), vec![file("a.rs", PATCH_2)])).await;
    // Finder deadline at 60 % = 1.2 s, well before the 2 s review timeout.
    s.engine.review_timeout = Duration::from_secs(2);
    s.chat
        .delay("<ReviewInstructions>", Duration::from_secs(30));
    let started = Instant::now();

    let err = run(&s, Trigger::Opened).await.unwrap_err();

    assert_eq!(err, EngineError::Fatal("review timed out".into()));
    assert!(started.elapsed() < Duration::from_millis(1900));
    assert!(s.git.state().reviews.is_empty());
    let row = latest(&pool).await;
    assert_eq!(row.status, ReviewStatus::Failed);
    assert_eq!(row.error.as_deref(), Some("review timed out"));
}

/// A batch whose only agent was cut by the Finder deadline is listed under Skipped files, so the
/// summary never implies its files were reviewed.
#[sqlx::test(migrations = "../../migrations")]
async fn finder_deadline_lists_the_unfinished_batch_as_skipped(pool: PgPool) {
    let files = vec![big_file("a.rs"), big_file("b.rs")];
    let mut s = setup(&pool, FakeGit::new(pr("h1"), files)).await;
    // Finder deadline at 60 % = 1.8 s; the b.rs batch would answer after 30 s.
    s.engine.review_timeout = Duration::from_secs(3);
    s.chat.delay(&diff_header("b.rs"), Duration::from_secs(30));
    s.chat.push_for(
        Stage::Finder,
        &diff_header("a.rs"),
        Reply::text(&finding("a.rs", 2, "Bug")),
    );

    let id = run(&s, Trigger::Opened).await.unwrap();

    let row = review(&pool, id).await;
    assert_eq!(row.status, ReviewStatus::Done);
    assert_eq!(row.comments_posted, 1);
    let git = s.git.state();
    let summary = &git.reviews[0].summary;
    assert!(
        summary.contains("**Skipped files**\n- `b.rs`: review did not finish in time"),
        "{summary}"
    );
    assert!(!summary.contains("`a.rs`"), "{summary}");
    assert!(
        summary.contains("**Notes**\n- Generalist reviewer did not finish in time"),
        "{summary}"
    );
}

/// Spec decision 12: the clone counts against the Finder deadline. A clone still running then
/// is abandoned and the Finder falls back to the diff.
#[sqlx::test(migrations = "../../migrations")]
async fn clone_is_bounded_by_the_finder_deadline(pool: PgPool) {
    let mut s = setup(&pool, FakeGit::new(pr("h1"), vec![file("a.rs", PATCH_2)])).await;
    s.git.state().clone_spec = Some(clone_spec());
    // Finder deadline at 60 % = 1.2 s; the clone would take 30 s.
    s.engine.review_timeout = Duration::from_secs(2);
    s.cloner.delay(Duration::from_secs(30));
    let started = Instant::now();

    let err = run(&s, Trigger::Opened).await.unwrap_err();

    // The clone used up the Finder budget, so the diff-only Finder is cut at once.
    assert_eq!(err, EngineError::Fatal("review timed out".into()));
    assert!(started.elapsed() < Duration::from_millis(1900));
    assert_eq!(s.cloner.urls().len(), 1);
    let finder = s.chat.finder_calls();
    assert!(
        !finder.is_empty(),
        "the Finder ran after the clone timed out"
    );
    for call in &finder {
        assert_eq!(call.tools, vec!["submit_findings"], "no repository tools");
        assert!(call.system.contains("You cannot open other files"));
    }
}

async fn set_review_mode(pool: &PgPool, mode: ReviewMode) {
    let mut current = settings::get(pool).await.unwrap();
    current.review_mode = mode;
    settings::update(pool, &current).await.unwrap();
}

#[sqlx::test(migrations = "../../migrations")]
async fn specialists_review_uses_the_30_minute_budget(pool: PgPool) {
    assert_eq!(SPECIALISTS_REVIEW_TIMEOUT, Duration::from_secs(30 * 60));
    let mut s = setup(&pool, FakeGit::new(pr("h1"), vec![file("a.rs", PATCH_2)])).await;
    // No Generalist budget at all: a Specialists review can only finish on its own budget.
    s.engine.review_timeout = Duration::ZERO;

    // `@adudu review deep`.
    s.chat.push_for(
        Stage::Finder,
        BUG_ROLE,
        Reply::text(&finding("a.rs", 2, "Bug")),
    );
    let id = run_mode(&s, Trigger::Command, Some(ReviewMode::Specialists))
        .await
        .unwrap();
    let row = review(&pool, id).await;
    assert_eq!(row.status, ReviewStatus::Done);
    assert_eq!(row.comments_posted, 1);

    // Specialists as the settings default.
    set_review_mode(&pool, ReviewMode::Specialists).await;
    let id = run(&s, Trigger::Opened).await.unwrap();
    assert_eq!(review(&pool, id).await.status, ReviewStatus::Done);

    // Control: a Generalist review on the same engine is cut at once by its own budget.
    let err = run_mode(&s, Trigger::Command, Some(ReviewMode::Generalist))
        .await
        .unwrap_err();
    assert_eq!(err, EngineError::Fatal("review timed out".into()));
    let row = latest(&pool).await;
    assert_eq!(row.status, ReviewStatus::Failed);
    assert_eq!(row.error.as_deref(), Some("review timed out"));
}

#[sqlx::test(migrations = "../../migrations")]
async fn generalist_and_small_pr_keep_the_12_minute_budget(pool: PgPool) {
    assert_eq!(REVIEW_TIMEOUT, Duration::from_secs(12 * 60));
    let mut s = setup(&pool, FakeGit::new(pr("h1"), vec![file("a.rs", PATCH_2)])).await;
    // No Specialists budget at all: everything below only finishes on the Generalist budget.
    s.engine.specialists_review_timeout = Duration::ZERO;

    // Generalist as the settings default.
    let id = run(&s, Trigger::Opened).await.unwrap();
    assert_eq!(review(&pool, id).await.status, ReviewStatus::Done);

    // `@adudu review generalist` over a Specialists default.
    set_review_mode(&pool, ReviewMode::Specialists).await;
    let id = run_mode(&s, Trigger::Command, Some(ReviewMode::Generalist))
        .await
        .unwrap();
    assert_eq!(review(&pool, id).await.status, ReviewStatus::Done);

    // A small PR under a Specialists default is forced to Generalist, budget included.
    enable_small_pr(&pool).await;
    let before = s.chat.finder_calls().len();
    let id = run(&s, Trigger::Opened).await.unwrap();
    assert_eq!(review(&pool, id).await.status, ReviewStatus::Done);
    let finder = s.chat.finder_calls();
    assert_eq!(finder.len() - before, 1);
    assert!(finder[before].system.contains(GENERALIST_ROLE));

    // Control: a full Specialists review is cut at once by its own budget.
    let err = run_mode(&s, Trigger::Command, Some(ReviewMode::Specialists))
        .await
        .unwrap_err();
    assert_eq!(err, EngineError::Fatal("review timed out".into()));
    assert_eq!(latest(&pool).await.status, ReviewStatus::Failed);
}

/// The live failure this guards against: a slow specialist cut by the Finder deadline lost all
/// its work. The agent now wraps up when the deadline gets close and its findings are posted.
#[sqlx::test(migrations = "../../migrations")]
async fn slow_specialist_submits_before_the_finder_deadline(pool: PgPool) {
    let mut s = setup(&pool, FakeGit::new(pr("h1"), vec![file("a.rs", PATCH_2)])).await;
    s.git.state().clone_spec = Some(clone_spec());
    s.cloner.files(&[("a.rs", "a\nb\n")]);
    // Finder deadline at 60 % = 2.4 s. The Bug reviewer's first step takes 600 ms and every
    // later one 100 ms, and it keeps reading files while it may: running all 20 steps would end
    // at ~2.5 s, after the deadline. With less than 2 × 600 ms left it must submit instead
    // (around 1.3 s), leaving about a second of margin for a loaded host.
    s.engine.specialists_review_timeout = Duration::from_secs(4);
    s.chat.explore_until_wrap_up(
        BUG_ROLE,
        "a.rs",
        json!([finding_json("a.rs", 2, "warning", "Slow bug")]),
        Duration::from_millis(600),
        Duration::from_millis(100),
    );

    let id = run_mode(&s, Trigger::Command, Some(ReviewMode::Specialists))
        .await
        .unwrap();

    let row = review(&pool, id).await;
    assert_eq!(row.status, ReviewStatus::Done);
    assert_eq!(row.comments_posted, 1);
    let git = s.git.state();
    let posted = &git.reviews[0];
    assert!(posted.comments[0].body.contains("Slow bug"));
    assert!(
        !posted.summary.contains("did not finish in time"),
        "{}",
        posted.summary
    );
    let bug: Vec<_> = s
        .chat
        .finder_calls()
        .into_iter()
        .filter(|c| c.system.contains(BUG_ROLE))
        .collect();
    // It investigated, then wrapped up early on time, not in the last steps of its budget.
    assert!(bug.len() >= 2, "{} steps", bug.len());
    assert!(bug.len() < FINDER_MAX_STEPS - 2, "{} steps", bug.len());
    let (last, earlier) = bug.split_last().unwrap();
    assert_eq!(last.tools, vec!["submit_findings"]);
    assert!(
        earlier
            .iter()
            .all(|c| c.tools.contains(&"read_file".to_string()))
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn multiline_finding_keeps_same_hunk_span_and_drops_cross_hunk_start_line(pool: PgPool) {
    let patch = "@@ -1,2 +1,4 @@\n ctx\n+let a = 1;\n+let b = 2;\n tail\n\
                 @@ -10,2 +12,3 @@\n x\n+let y = 3;\n z\n";
    let s = setup(&pool, FakeGit::new(pr("h1"), vec![file("src/a.rs", patch)])).await;
    let findings = json!([
        {
            "file": "src/a.rs",
            "start_line": 2,
            "line": 3,
            "severity": "warning",
            "title": "Same hunk",
            "body": "spans lines 2-3",
            "suggestion": "let a = 10;\nlet b = 20;",
        },
        {
            "file": "src/a.rs",
            "start_line": 2,
            "line": 13,
            "severity": "info",
            "title": "Cross hunk",
            "body": "cannot span across hunks",
            "suggestion": "let a = 1;\nlet b = 2;\nlet y = 4;",
        },
    ]);
    s.chat.push(Ok(&findings.to_string()));
    s.chat
        .push_stage(Stage::Dedup, Reply::text(r#"{"keep_indices": [0, 1]}"#));

    let id = run(&s, Trigger::Opened).await.unwrap();

    let row = review(&pool, id).await;
    assert_eq!(row.status, ReviewStatus::Done);
    assert_eq!(row.comments_posted, 2);

    let verifier = s.chat.verifier_calls();
    assert_eq!(verifier.len(), 2);
    assert!(
        verifier
            .iter()
            .any(|c| c.user.contains("\"start_line\": 2")),
        "same-hunk finding retains start_line for verifier"
    );

    let git = s.git.state();
    let posted = &git.reviews[0].comments;
    assert_eq!(posted.len(), 2);
    assert_eq!(posted[0].start_line, Some(2));
    assert_eq!(posted[0].line, 3);
    assert_eq!(posted[0].end_offset, Some(11));
    assert!(
        posted[0]
            .body
            .ends_with("```suggestion\nlet a = 10;\nlet b = 20;\n```"),
        "{}",
        posted[0].body
    );
    assert_eq!(posted[1].start_line, None);
    assert_eq!(posted[1].line, 13);
    assert_eq!(posted[1].end_offset, None);
    // The dropped span takes its multi-line suggestion with it: posted on line 13 alone it
    // would duplicate lines on "Apply suggestion".
    assert!(posted[1].body.contains("cannot span across hunks"));
    assert!(
        !posted[1].body.contains("```suggestion"),
        "{}",
        posted[1].body
    );
}

/// Incremental review: ranges are checked against the hunks shared by the new commit's diff
/// and the PR diff (`intersect_hunks`), not against the wider commit hunk.
#[sqlx::test(migrations = "../../migrations")]
async fn incremental_review_keeps_ranges_inside_the_intersected_hunk_only(pool: PgPool) {
    let s = setup(
        &pool,
        FakeGit::new(pr("h1"), vec![file("src/a.rs", PATCH_2)]),
    )
    .await;
    run(&s, Trigger::Opened).await.unwrap();
    {
        let mut git = s.git.state();
        git.pr = pr("h2");
        // PR diff: two hunks, new lines 1-4 and 10-13 (added: 2, 3, 11, 12).
        git.full = vec![file(
            "src/a.rs",
            "@@ -1,2 +1,4 @@\n l1\n+l2\n+l3\n l4\n@@ -8,2 +10,4 @@\n l10\n+l11\n+l12\n l13\n",
        )];
        // New commit: one wide hunk, new lines 1-13, adding the same lines.
        git.incremental = vec![file(
            "src/a.rs",
            "@@ -1,9 +1,13 @@\n l1\n+l2\n+l3\n l4\n l5\n l6\n l7\n l8\n l9\n l10\n+l11\n+l12\n l13\n",
        )];
    }
    let findings = json!([
        {
            "file": "src/a.rs", "start_line": 2, "line": 3, "severity": "warning",
            "title": "Inside", "body": "inside the PR hunk", "suggestion": "L2\nL3",
        },
        {
            "file": "src/a.rs", "start_line": 3, "line": 11, "severity": "warning",
            "title": "Crossing", "body": "leaves the PR hunk", "suggestion": "x\ny\nz",
        },
    ]);
    s.chat.push(Ok(&findings.to_string()));

    let id = run(&s, Trigger::Updated).await.unwrap();

    assert_eq!(review(&pool, id).await.status, ReviewStatus::Done);
    let git = s.git.state();
    assert_eq!(git.diff_between_calls.len(), 1, "incremental path taken");
    let posted = &git.reviews[1].comments;
    assert_eq!(posted.len(), 2);
    assert_eq!((posted[0].start_line, posted[0].line), (Some(2), 3));
    assert_eq!(posted[0].end_offset, Some(3));
    assert!(posted[0].body.ends_with("```suggestion\nL2\nL3\n```"));
    assert_eq!((posted[1].start_line, posted[1].line), (None, 11));
    assert_eq!(posted[1].end_offset, None);
    assert!(
        !posted[1].body.contains("```suggestion"),
        "{}",
        posted[1].body
    );
}

/// Two specialists report the same `(file, line, title)`; the one with a valid range is kept.
#[sqlx::test(migrations = "../../migrations")]
async fn exact_duplicates_prefer_the_entry_with_a_valid_range(pool: PgPool) {
    let s = setup(
        &pool,
        FakeGit::new(pr("h1"), vec![file("src/a.rs", PATCH_234)]),
    )
    .await;
    s.chat.push_for(
        Stage::Finder,
        BUG_ROLE,
        Reply::text(&json!([finding_json("src/a.rs", 3, "warning", "Dup")]).to_string()),
    );
    let ranged = json!([{
        "file": "src/a.rs", "start_line": 2, "line": 3, "severity": "warning",
        "title": "Dup", "body": "ranged", "suggestion": "B\nC",
    }]);
    s.chat.push_for(
        Stage::Finder,
        SECURITY_ROLE,
        Reply::text(&ranged.to_string()),
    );

    run_mode(&s, Trigger::Opened, Some(ReviewMode::Specialists))
        .await
        .unwrap();

    assert_eq!(
        s.chat.verifier_calls().len(),
        1,
        "one candidate after exact dedup"
    );
    let git = s.git.state();
    let posted = &git.reviews[0].comments;
    assert_eq!(posted.len(), 1);
    assert_eq!((posted[0].start_line, posted[0].line), (Some(2), 3));
    assert!(posted[0].body.contains("ranged"));
}

/// Sets `settings.max_review_input_tokens`. Every `FakeChat` answer reports 1 input token.
async fn set_token_limit(pool: &PgPool, limit: i32) {
    let mut current = settings::get(pool).await.unwrap();
    current.max_review_input_tokens = limit;
    settings::update(pool, &current).await.unwrap();
}

/// The summary note when the token ceiling cut something: `details` lists what it cut.
fn budget_warning(used: u64, limit: u64, details: &str) -> String {
    format!("Token budget reached ({used} of {limit} input tokens): {details}.")
}

#[sqlx::test(migrations = "../../migrations")]
async fn batches_not_started_after_budget_exhausted_are_skipped_as_token_budget(pool: PgPool) {
    let files = vec![big_file("a.rs"), big_file("b.rs"), big_file("c.rs")];
    let s = setup(&pool, FakeGit::new(pr("h1"), files)).await;
    set_token_limit(&pool, 1).await;
    let id = run(&s, Trigger::Opened).await.unwrap();

    let row = review(&pool, id).await;
    assert_eq!(row.status, ReviewStatus::Done);
    // Batches a.rs and b.rs start together (BATCH_CONCURRENCY 2) before any token is
    // counted; c.rs would start after one of them answered, past the ceiling.
    let finder = s.chat.finder_calls();
    assert_eq!(finder.len(), 2);
    assert!(
        finder
            .iter()
            .all(|c| !c.user.contains(&diff_header("c.rs")))
    );
    let git = s.git.state();
    let summary = &git.reviews[0].summary;
    assert!(
        summary.contains("**Skipped files**\n- `c.rs`: token budget reached"),
        "{summary}"
    );
    assert!(!summary.contains("`a.rs`"), "{summary}");
    assert!(
        summary.contains(&budget_warning(2, 1, "1 batch not reviewed")),
        "{summary}"
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn all_failed_with_a_token_budget_skip_is_fatal(pool: PgPool) {
    let files = vec![big_file("a.rs"), big_file("b.rs"), big_file("c.rs")];
    let s = setup(&pool, FakeGit::new(pr("h1"), files)).await;
    set_token_limit(&pool, 1).await;
    // a.rs and b.rs answer prose (1 token each, past the ceiling), then fail retryably after
    // the nudge; c.rs is never started.
    for path in ["a.rs", "b.rs"] {
        let needle = diff_header(path);
        s.chat
            .push_for(Stage::Finder, &needle, Reply::text("thinking"));
        s.chat.push_for(
            Stage::Finder,
            &needle,
            Reply::Err(ChatError::AllKeysFailed { retry_after: None }),
        );
    }

    let err = run(&s, Trigger::Opened).await.unwrap_err();

    assert!(
        matches!(&err, EngineError::Fatal(m) if m == "review stopped: token budget reached"),
        "{err:?}"
    );
    let row = latest(&pool).await;
    assert_eq!(row.status, ReviewStatus::Failed);
    assert_eq!(
        row.error.as_deref(),
        Some("review stopped: token budget reached")
    );
    assert!(s.git.state().reviews.is_empty());
}

#[sqlx::test(migrations = "../../migrations")]
async fn exhausted_budget_verifies_remaining_candidates_diff_only(pool: PgPool) {
    let patch = "@@ -1,1 +1,6 @@\n a\n+b\n+c\n+d\n+e\n+f\n";
    let s = setup(&pool, FakeGit::new(pr("h1"), vec![file("a.rs", patch)])).await;
    s.git.state().clone_spec = Some(clone_spec());
    s.cloner.files(&[("a.rs", "a\nb\nc\nd\ne\nf\n")]);
    // The Finder's single call spends the whole ceiling of 1, before the Verifier starts.
    set_token_limit(&pool, 1).await;
    let findings: Vec<_> = (2..=6)
        .map(|line| finding_json("a.rs", line, "warning", &format!("Finding {line}")))
        .collect();
    s.chat
        .push_stage(Stage::Finder, Reply::submit(json!(findings)));

    let id = run(&s, Trigger::Opened).await.unwrap();

    let row = review(&pool, id).await;
    assert_eq!(row.status, ReviewStatus::Done);
    assert_eq!(
        row.comments_posted, 5,
        "every finding is verified and posted"
    );
    // One diff-only call per candidate instead of a tool loop.
    let verifier = s.chat.verifier_calls();
    assert_eq!(verifier.len(), 5);
    for call in &verifier {
        assert!(call.tools.is_empty(), "{:?}", call.tools);
        assert!(call.system.contains("ONLY a JSON object"));
        assert_eq!(call.model, "gpt-fast");
    }
    assert_eq!(s.chat.dedup_calls().len(), 1);
    let git = s.git.state();
    let summary = &git.reviews[0].summary;
    assert!(!summary.contains("could not be verified"), "{summary}");
    // Finder 1 + Verifier 5 + Dedup 1 input tokens.
    assert!(
        summary.contains(&budget_warning(
            7,
            1,
            "5 findings verified from the diff only"
        )),
        "{summary}"
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn tool_enabled_verifier_wraps_up_when_the_token_budget_runs_out(pool: PgPool) {
    let s = setup(
        &pool,
        FakeGit::new(pr("h1"), vec![file("src/a.rs", PATCH_2)]),
    )
    .await;
    s.git.state().clone_spec = Some(clone_spec());
    s.cloner.files(&[("src/a.rs", "a\nb\n")]);
    // Finder 1 token: the Verifier starts under the ceiling of 2 with its tools, and its
    // first step reaches it.
    set_token_limit(&pool, 2).await;
    s.chat.push_stage(
        Stage::Finder,
        Reply::submit(json!([finding_json(
            "src/a.rs",
            2,
            "warning",
            "Off by one"
        )])),
    );
    s.chat.push_for(
        Stage::Verifier,
        "Off by one",
        Reply::tool("read_file", json!({"path": "src/a.rs"})),
    );

    let id = run(&s, Trigger::Opened).await.unwrap();

    assert_eq!(review(&pool, id).await.comments_posted, 1);
    let verifier = s.chat.verifier_calls();
    assert_eq!(verifier.len(), 2);
    assert!(verifier[0].tools.contains(&"read_file".to_string()));
    assert_eq!(verifier[1].tools, vec!["submit_verdict"], "wrapped up");
    // A Verifier wrap-up is none of the cuts the summary names.
    let git = s.git.state();
    let summary = &git.reviews[0].summary;
    assert!(!summary.contains("Token budget reached"), "{summary}");
}

#[sqlx::test(migrations = "../../migrations")]
async fn finder_wrap_up_for_token_budget_is_named_in_the_warning(pool: PgPool) {
    let s = setup(
        &pool,
        FakeGit::new(pr("h1"), vec![file("src/a.rs", PATCH_2)]),
    )
    .await;
    s.git.state().clone_spec = Some(clone_spec());
    s.cloner.files(&[("src/a.rs", "a\nb\n")]);
    set_token_limit(&pool, 1).await;
    s.chat.explore_until_wrap_up(
        GENERALIST_ROLE,
        "src/a.rs",
        json!([finding_json("src/a.rs", 2, "warning", "Bug")]),
        Duration::ZERO,
        Duration::ZERO,
    );

    let id = run(&s, Trigger::Opened).await.unwrap();

    assert_eq!(review(&pool, id).await.comments_posted, 1);
    // One investigating step reaches the ceiling; the next one only submits.
    let finder = s.chat.finder_calls();
    assert_eq!(finder.len(), 2);
    assert_eq!(finder[1].tools, vec!["submit_findings"]);
    let git = s.git.state();
    let summary = &git.reviews[0].summary;
    assert!(
        summary.contains(&budget_warning(
            3,
            1,
            "some reviewers stopped early, 1 finding verified from the diff only"
        )),
        "{summary}"
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn budget_warning_only_when_something_was_cut(pool: PgPool) {
    let s = setup(&pool, FakeGit::new(pr("h1"), vec![file("a.rs", PATCH_234)])).await;

    // 0 = no limit: no warning, the finding is verified and posted.
    set_token_limit(&pool, 0).await;
    s.chat.push(Ok(&finding("a.rs", 2, "Bug")));
    run(&s, Trigger::Opened).await.unwrap();

    // Finder 1 + Verifier 1: only the last call crosses the ceiling of 2.
    set_token_limit(&pool, 2).await;
    s.chat.push(Ok(&finding("a.rs", 2, "Bug")));
    run(&s, Trigger::Opened).await.unwrap();

    // The Finder alone reaches the ceiling of 1, but the Verifier was diff-only anyway (no
    // checkout): nothing changes, so nothing is reported.
    set_token_limit(&pool, 1).await;
    s.chat.push(Ok(&finding("a.rs", 2, "Bug")));
    run(&s, Trigger::Opened).await.unwrap();

    assert_eq!(s.chat.verifier_calls().len(), 3);
    let git = s.git.state();
    assert_eq!(git.reviews.len(), 3);
    for posted in &git.reviews {
        assert_eq!(posted.comments.len(), 1);
        assert!(
            !posted.summary.contains("Token budget reached"),
            "{}",
            posted.summary
        );
        assert!(!posted.summary.contains("**Notes**"), "{}", posted.summary);
    }
}

/// Obviously fake credentials in the provider formats.
const FAKE_AWS_KEY: &str = "AKIAIOSFODNN7EXAMPLE";
const FAKE_GITHUB_TOKEN: &str = "ghp_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

#[sqlx::test(migrations = "../../migrations")]
async fn posted_comment_body_is_redacted(pool: PgPool) {
    let s = setup(
        &pool,
        FakeGit::new(pr("h1"), vec![file("src/a.rs", PATCH_2)]),
    )
    .await;
    let findings = json!([{
        "file": "src/a.rs", "line": 2, "severity": "critical",
        "title": format!("Hardcoded key {FAKE_AWS_KEY}"),
        "body": format!("The token `{FAKE_GITHUB_TOKEN}` is committed; load it from the environment."),
    }]);
    s.chat.push(Ok(&findings.to_string()));

    run(&s, Trigger::Opened).await.unwrap();

    let git = s.git.state();
    let posted = &git.reviews[0].comments;
    assert_eq!(posted.len(), 1);
    let body = &posted[0].body;
    assert!(body.starts_with(ADUDU_MARKER), "{body}");
    assert!(
        !body.contains(FAKE_AWS_KEY) && !body.contains(FAKE_GITHUB_TOKEN),
        "{body}"
    );
    assert!(body.contains("Hardcoded key [REDACTED]"), "{body}");
    assert!(
        body.contains("The token `[REDACTED]` is committed"),
        "{body}"
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn suggestion_containing_a_secret_is_dropped(pool: PgPool) {
    let s = setup(
        &pool,
        FakeGit::new(pr("h1"), vec![file("src/a.rs", PATCH_234)]),
    )
    .await;
    let findings = json!([
        {
            "file": "src/a.rs", "start_line": 2, "line": 3, "severity": "critical",
            "title": "Leaked key", "body": "move it to the environment",
            "suggestion": format!("let key = \"{FAKE_AWS_KEY}\";\nlet c = 1;"),
        },
        {
            "file": "src/a.rs", "line": 4, "severity": "info",
            "title": "Clean", "body": "pass the settings value",
            // Keyword arguments name a variable, not a secret: the block stays.
            "suggestion": "    password=password,\n    api_key=settings.API_KEY,",
        },
    ]);
    s.chat.push(Ok(&findings.to_string()));
    s.chat
        .push_stage(Stage::Dedup, Reply::text(r#"{"keep_indices": [0, 1]}"#));

    run(&s, Trigger::Opened).await.unwrap();

    let git = s.git.state();
    let posted = &git.reviews[0].comments;
    assert_eq!(posted.len(), 2);
    // The comment keeps its span; only the suggestion block goes.
    assert_eq!(posted[0].start_line, Some(2));
    assert_eq!(posted[0].line, 3);
    assert!(posted[0].body.contains("move it to the environment"));
    assert!(
        !posted[0].body.contains("```suggestion"),
        "{}",
        posted[0].body
    );
    assert!(!posted[0].body.contains(FAKE_AWS_KEY), "{}", posted[0].body);
    assert!(!posted[0].body.contains("[REDACTED]"), "{}", posted[0].body);
    assert!(
        posted[0].body.ends_with(
            "move it to the environment\n\n_Suggested code omitted: it contained a value that looks like a secret._"
        ),
        "{}",
        posted[0].body
    );
    assert!(!posted[1].body.contains("Suggested code omitted"));
    assert!(
        posted[1]
            .body
            .ends_with("```suggestion\n    password=password,\n    api_key=settings.API_KEY,\n```"),
        "{}",
        posted[1].body
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn summary_body_is_redacted(pool: PgPool) {
    let secret_path = format!("keys/{FAKE_AWS_KEY}.bin");
    let mut binary = file(&secret_path, "");
    binary.patch = None;
    let s = setup(
        &pool,
        FakeGit::new(pr("h1"), vec![binary, file("ok.rs", PATCH_2)]),
    )
    .await;
    set_ignore(&pool, Some(&[]), &[]).await;

    run(&s, Trigger::Opened).await.unwrap();

    let summary = s.git.state().reviews[0].summary.clone();
    assert!(summary.starts_with(ADUDU_MARKER), "{summary}");
    assert!(!summary.contains(FAKE_AWS_KEY), "{summary}");
    assert!(
        summary.contains("`keys/[REDACTED].bin`: no diff available"),
        "{summary}"
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn finder_still_sees_the_raw_diff(pool: PgPool) {
    let patch = format!("@@ -1,1 +1,2 @@\n a\n+let key = \"{FAKE_AWS_KEY}\";\n");
    let s = setup(
        &pool,
        FakeGit::new(pr("h1"), vec![file("src/a.rs", &patch)]),
    )
    .await;
    s.git.state().clone_spec = Some(clone_spec());
    let content = format!("a\nlet key = \"{FAKE_AWS_KEY}\";\n");
    s.cloner.files(&[("src/a.rs", content.as_str())]);
    s.chat.push_stage(
        Stage::Finder,
        Reply::tool("read_file", json!({"path": "src/a.rs"})),
    );
    s.chat.push_stage(
        Stage::Finder,
        Reply::submit(json!([finding_json(
            "src/a.rs",
            2,
            "critical",
            "Hardcoded key"
        )])),
    );

    run(&s, Trigger::Command).await.unwrap();

    let finder = s.chat.finder_calls();
    assert!(
        finder[0].user.contains(FAKE_AWS_KEY),
        "the diff is not redacted"
    );
    assert!(
        finder[0]
            .system
            .contains("`[REDACTED]` in tool output marks a value Adus masked"),
        "{}",
        finder[0].system
    );
    // Tool output is masked.
    let last = finder.last().unwrap();
    assert!(
        last.tool_results
            .iter()
            .any(|r| r.contains("2: let key = \"[REDACTED]\";")),
        "{:?}",
        last.tool_results
    );
    assert!(last.tool_results.iter().all(|r| !r.contains(FAKE_AWS_KEY)));
    // The Verifier sees the raw diff and the raw snippet around the finding.
    let verifier = s.chat.verifier_calls();
    assert_eq!(verifier.len(), 1);
    assert!(
        verifier[0]
            .user
            .contains(&format!("2: let key = \"{FAKE_AWS_KEY}\";"))
    );
    assert!(
        verifier[0]
            .system
            .contains("`[REDACTED]` in tool output marks a value Adus masked")
    );
    // The posted comment never carries the key.
    let git = s.git.state();
    assert!(!git.reviews[0].comments[0].body.contains(FAKE_AWS_KEY));
}
