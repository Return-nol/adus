mod common;

use std::time::Duration;

use adus_core::job::Trigger;
use adus_core::types::{ADUDU_MARKER, Severity};
use adus_db::connections::{self, ConnectionStatus};
use adus_db::repositories::{self, RepoPatch};
use adus_db::reviews::{self, Review, ReviewStatus};
use adus_db::rules::{self, KnowledgeScope, NewRule};
use adus_db::settings;
use adus_git::{ConnectionKind, FileStatus, GitError};
use adus_llm::router::ChatError;
use adus_review::engine::EngineError;
use common::{FakeGit, PAT, Setup, file, finding, pr, setup};
use sqlx::PgPool;
use uuid::Uuid;

/// Tiga baris ditambah: 2, 3, 4.
const PATCH_234: &str = "@@ -1,1 +1,4 @@\n a\n+b\n+c\n+d\n";
/// Satu baris ditambah: 2.
const PATCH_2: &str = "@@ -1,1 +1,2 @@\n a\n+b\n";

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

async fn run(s: &Setup, trigger: Trigger) -> Result<Uuid, EngineError> {
    s.engine.review_pr(s.repository_id, 7, trigger).await
}

#[sqlx::test(migrations = "../../migrations")]
async fn full_review_posts_valid_comments_and_summary(pool: PgPool) {
    let s = setup(
        &pool,
        FakeGit::new(pr("h1"), vec![file("src/a.rs", PATCH_234)]),
    )
    .await;
    rules::insert(
        &pool,
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

    let calls = s.chat.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].provider_id, s.provider_id);
    assert_eq!(calls[0].model, "gpt-test");
    assert_eq!(calls[0].max_tokens, 4096);
    assert!(calls[0].user.contains("No unwrap"));
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
        .review_pr(Uuid::new_v4(), 7, Trigger::Command)
        .await;
    assert_eq!(missing, Ok(Uuid::nil()));
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
    let calls = s.chat.calls();
    assert_eq!(calls.len(), 2, "first run 1 file, incremental run 1 file");
    assert!(calls[1].user.contains("<file_path>\na.rs\n</file_path>"));
    assert!(
        calls[1].user.contains("4 + d"),
        "prompt has the new-commit patch"
    );
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

    run(&s, Trigger::Opened).await.unwrap();

    let calls = s.chat.calls();
    assert_eq!(calls.len(), 1);
    assert!(calls[0].user.contains("<file_path>\nok.rs\n</file_path>"));
    let summary = s.git.state().reviews[0].summary.clone();
    assert!(summary.contains("`big.rs`: patch too large"));
    assert!(summary.contains("`logo.png`: binary file"));
    assert!(!summary.contains("gone.rs"));
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

    assert_eq!(s.chat.calls().len(), 2);
    let summary = s.git.state().reviews[0].summary.clone();
    assert!(summary.contains("`c.rs`: file limit reached"));
    assert!(!summary.contains("`a.rs`"));
}

#[sqlx::test(migrations = "../../migrations")]
async fn unparseable_output_retried_once_then_skipped(pool: PgPool) {
    let files = vec![file("a.rs", PATCH_2), file("b.rs", PATCH_2)];
    let s = setup(&pool, FakeGit::new(pr("h1"), files)).await;
    s.chat.push(Ok("I think it's fine"));
    s.chat.push(Ok(&finding("a.rs", 2, "Second try")));
    s.chat.push(Ok("nope"));
    s.chat.push(Ok("still nope"));

    let id = run(&s, Trigger::Opened).await.unwrap();

    assert_eq!(s.chat.calls().len(), 4);
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
    let files = vec![file("a.rs", PATCH_2), file("b.rs", PATCH_2)];
    let s = setup(&pool, FakeGit::new(pr("h1"), files)).await;
    s.chat.push(Ok(&finding("a.rs", 2, "Bug")));
    s.chat.push(Err(ChatError::AllKeysFailed {
        retry_after: Some(Duration::from_secs(30)),
    }));

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
    let rows = reviews::list(&pool, None, 1, 10).await.unwrap();
    assert_eq!(rows[0].status, ReviewStatus::Failed);
    assert!(rows[0].error.is_some());

    s.chat.push(Err(ChatError::NoKeys));
    assert!(matches!(
        run(&s, Trigger::Opened).await,
        Err(EngineError::Retryable(_, None))
    ));
    s.chat.push(Err(ChatError::Invalid("bad model".into())));
    assert!(matches!(
        run(&s, Trigger::Opened).await,
        Err(EngineError::Fatal(_))
    ));
}

#[sqlx::test(migrations = "../../migrations")]
async fn no_model_configured_fails_with_pr_comment(pool: PgPool) {
    let s = setup(&pool, FakeGit::new(pr("h1"), vec![file("a.rs", PATCH_2)])).await;
    let mut current = settings::get(&pool).await.unwrap();
    current.default_provider_id = None;
    current.default_model = None;
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
async fn repo_model_override_wins(pool: PgPool) {
    let s = setup(&pool, FakeGit::new(pr("h1"), vec![file("a.rs", PATCH_2)])).await;
    let other =
        adus_db::ai::insert_provider(&pool, adus_core::types::ProviderKind::Anthropic, "a", None)
            .await
            .unwrap();
    let patch = RepoPatch {
        review_provider_id: Some(Some(other.id)),
        review_model: Some(Some("claude-test".into())),
        ..Default::default()
    };
    repositories::update(&pool, s.repository_id, patch)
        .await
        .unwrap();

    run(&s, Trigger::Opened).await.unwrap();

    let calls = s.chat.calls();
    assert_eq!(calls[0].provider_id, other.id);
    assert_eq!(calls[0].model, "claude-test");
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
    s.git.state().post_error = Some(GitError::Http("502".into()));
    assert!(matches!(
        run(&s, Trigger::Opened).await,
        Err(EngineError::Retryable(_, None))
    ));

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

    let id = run(&s, Trigger::Opened).await.unwrap();

    let calls = s.chat.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].ctx.review_id, Some(id));
    assert_eq!(calls[0].ctx.repository_id, Some(s.repository_id));
}
