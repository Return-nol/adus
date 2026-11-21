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

/// Review PR 7 di repo hasil `setup`; repo itu ada, jadi selalu ada baris review.
async fn run(s: &Setup, trigger: Trigger) -> Result<Uuid, EngineError> {
    let result = s.engine.review_pr(s.repository_id, 7, trigger, None).await;
    result.map(|id| id.expect("review row"))
}

async fn latest(pool: &PgPool) -> Review {
    reviews::list(pool, None, 1, 1).await.unwrap().remove(0)
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
    assert_eq!(calls[0].max_tokens, 8192);
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
    // Daftar bawaan mengabaikan *.png; test ini menguji jalur `no_patch`.
    set_ignore(&pool, Some(&[]), &[]).await;

    run(&s, Trigger::Opened).await.unwrap();

    let calls = s.chat.calls();
    assert_eq!(calls.len(), 1);
    assert!(calls[0].user.contains("<file_path>\nok.rs\n</file_path>"));
    let summary = s.git.state().reviews[0].summary.clone();
    assert!(summary.contains("`big.rs`: patch too large"));
    assert!(summary.contains("`logo.png`: no diff available (binary or too large)"));
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
    // Kedua file berjalan bersamaan: panggilan pertama a.rs, lalu b.rs, lalu ulangan keduanya.
    s.chat.delay("b.rs", Duration::from_millis(50));
    s.chat.push(Ok("I think it's fine"));
    s.chat.push(Ok("nope"));
    s.chat.push(Ok(&finding("a.rs", 2, "Second try")));
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
    // Retryable: barisnya dihapus, percobaan ulang membuat baris baru.
    assert!(reviews::list(&pool, None, 1, 10).await.unwrap().is_empty());

    s.chat.push(Err(ChatError::NoKeys));
    assert!(matches!(
        run(&s, Trigger::Opened).await,
        Err(EngineError::Retryable(_, None))
    ));
    assert!(reviews::list(&pool, None, 1, 10).await.unwrap().is_empty());
}

#[sqlx::test(migrations = "../../migrations")]
async fn one_invalid_file_is_skipped_and_review_posted(pool: PgPool) {
    let files = vec![file("a.rs", PATCH_2), file("b.rs", PATCH_2)];
    let s = setup(&pool, FakeGit::new(pr("h1"), files)).await;
    s.chat
        .push(Err(ChatError::Invalid("empty response".into())));
    s.chat.push(Ok(&finding("b.rs", 2, "Bug")));

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
    let files = vec![file("a.rs", PATCH_2), file("b.rs", PATCH_2)];
    let s = setup(&pool, FakeGit::new(pr("h1"), files)).await;
    s.chat
        .push(Err(ChatError::ContextOverflow("prompt is too long".into())));
    s.chat.push(Ok(&finding("b.rs", 2, "Bug")));

    let id = run(&s, Trigger::Opened).await.unwrap();

    let row = review(&pool, id).await;
    assert_eq!(row.status, ReviewStatus::Done);
    assert_eq!(row.comments_posted, 1);
    let git = s.git.state();
    assert_eq!(git.reviews[0].comments[0].path, "b.rs");
    assert!(git.reviews[0].summary.contains("`a.rs`: LLM error"));
}

#[sqlx::test(migrations = "../../migrations")]
async fn tools_unsupported_is_fatal(pool: PgPool) {
    let s = setup(&pool, FakeGit::new(pr("h1"), vec![file("a.rs", PATCH_2)])).await;
    s.chat
        .push(Err(ChatError::ToolsUnsupported("no tools".into())));

    let err = run(&s, Trigger::Opened).await.unwrap_err();

    assert!(
        matches!(&err, EngineError::Fatal(m) if m.contains("tools not supported")),
        "{err:?}"
    );
    assert_eq!(latest(&pool).await.status, ReviewStatus::Failed);
}

#[sqlx::test(migrations = "../../migrations")]
async fn every_file_invalid_is_fatal(pool: PgPool) {
    let files = vec![file("a.rs", PATCH_2), file("b.rs", PATCH_2)];
    let s = setup(&pool, FakeGit::new(pr("h1"), files)).await;
    s.chat
        .push(Err(ChatError::Invalid("model not found".into())));
    s.chat
        .push(Err(ChatError::Invalid("model not found".into())));

    let err = run(&s, Trigger::Opened).await.unwrap_err();

    assert!(
        matches!(&err, EngineError::Fatal(m) if m.contains("model not found")),
        "{err:?}"
    );
    assert!(s.git.state().reviews.is_empty());
    let row = latest(&pool).await;
    assert_eq!(row.status, ReviewStatus::Failed);
}

#[sqlx::test(migrations = "../../migrations")]
async fn concurrent_files_keep_comment_order(pool: PgPool) {
    let paths = ["a.rs", "b.rs", "c.rs", "d.rs", "e.rs", "f.rs"];
    let files = paths.iter().map(|p| file(p, PATCH_2)).collect();
    let s = setup(&pool, FakeGit::new(pr("h1"), files)).await;
    for path in paths {
        s.chat.push(Ok(&finding(path, 2, path)));
    }
    // Yang pertama paling lambat: tanpa urutan terjaga, komentarnya akan terakhir.
    s.chat.delay("a.rs", Duration::from_millis(300));
    s.chat.delay("b.rs", Duration::from_millis(150));

    run(&s, Trigger::Opened).await.unwrap();

    let posted: Vec<String> = s.git.state().reviews[0]
        .comments
        .iter()
        .map(|c| c.path.clone())
        .collect();
    assert_eq!(posted, paths);
    assert!(
        s.chat.max_in_flight() > 1,
        "files are reviewed concurrently"
    );
    assert!(s.chat.max_in_flight() <= 4, "at most 4 at once");
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

    let id = run(&s, Trigger::Opened).await.unwrap();

    let calls = s.chat.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].ctx.review_id, Some(id));
    assert_eq!(calls[0].ctx.repository_id, Some(s.repository_id));
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

#[sqlx::test(migrations = "../../migrations")]
async fn slow_review_times_out_and_is_marked_failed(pool: PgPool) {
    let mut s = setup(
        &pool,
        FakeGit::new(pr("h1"), vec![file("src/a.rs", PATCH_234)]),
    )
    .await;
    s.git.state().get_pr_delay = Duration::from_secs(5);
    s.engine.review_timeout = Duration::from_millis(100);

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
    let calls = s.chat.calls();
    assert_eq!(calls.len(), 1);
    assert!(calls[0].user.contains("<file_path>\nb.rs\n</file_path>"));
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

    let calls = s.chat.calls();
    assert_eq!(calls.len(), 1);
    assert!(calls[0].user.contains("<file_path>\nb.rs\n</file_path>"));
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
    let calls = s.chat.calls();
    assert_eq!(calls.len(), 2, "first run a.rs, incremental run a.rs only");
    assert!(calls[1].user.contains("<file_path>\na.rs\n</file_path>"));
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
    let calls = s.chat.calls();
    assert_eq!(calls.len(), 1, "yarn.lock ignored by the defaults");
    assert!(calls[0].user.contains("<file_path>\na.rs\n</file_path>"));
}
