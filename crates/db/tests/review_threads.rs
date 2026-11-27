use adus_core::types::{ConnectionKind, Severity};
use adus_db::{
    connections,
    repositories::{self, NewRepository},
    review_threads::{self, NewReviewThread, ThreadOutcome, ThreadUpdate},
};
use sqlx::PgPool;

#[sqlx::test(migrations = "../../migrations")]
async fn review_threads_roundtrip(pool: PgPool) {
    let conn = connections::insert(
        &pool,
        ConnectionKind::Github,
        "c",
        "acme",
        "enc",
        "1234",
        "octocat",
    )
    .await
    .unwrap();
    let repo = repositories::insert(
        &pool,
        NewRepository {
            connection_id: conn.id,
            external_id: "1",
            owner: "acme",
            project: None,
            project_name: None,
            name: "api",
            default_branch: None,
            webhook_token: "t",
        },
    )
    .await
    .unwrap()
    .unwrap()
    .id;

    let row = |thread: &'static str, title: &'static str| NewReviewThread {
        repository_id: repo,
        review_id: None,
        pr_number: 7,
        provider_thread_id: thread,
        root_comment_id: thread,
        path: "src/lib.rs",
        line: Some(3),
        severity: Severity::Warning,
        title,
        body: "body",
        posted_head_sha: "h1",
    };
    // The second row repeats the thread id, so it is ignored.
    review_threads::insert_many(&pool, &[row("100", "first"), row("100", "duplicate")])
        .await
        .unwrap();

    let all = review_threads::for_pr(&pool, repo, 7).await.unwrap();
    assert_eq!(all.len(), 1);
    let thread = review_threads::find(&pool, repo, 7, "100")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(thread.title, "first");
    assert_eq!(thread.outcome, ThreadOutcome::Open);
    assert_eq!(thread.adus_replies, 0);
    assert_eq!(
        review_threads::open_for_pr(&pool, repo, 7)
            .await
            .unwrap()
            .len(),
        1
    );

    review_threads::set_outcome(
        &pool,
        thread.id,
        ThreadUpdate {
            outcome: ThreadOutcome::Fixed,
            reason: None,
            fix_sha: Some("abc"),
            resolution: Some("guarded"),
        },
    )
    .await
    .unwrap();
    assert!(
        review_threads::open_for_pr(&pool, repo, 7)
            .await
            .unwrap()
            .is_empty()
    );

    review_threads::record_reply(&pool, thread.id)
        .await
        .unwrap();
    review_threads::record_reply(&pool, thread.id)
        .await
        .unwrap();
    review_threads::set_validated(&pool, thread.id, "h2")
        .await
        .unwrap();
    let thread = review_threads::find(&pool, repo, 7, "100")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(thread.outcome, ThreadOutcome::Fixed);
    assert_eq!(thread.fix_sha.as_deref(), Some("abc"));
    assert_eq!(thread.resolution.as_deref(), Some("guarded"));
    assert_eq!(thread.adus_replies, 2);
    assert_eq!(thread.validated_head_sha.as_deref(), Some("h2"));
    assert!(thread.updated_at > thread.created_at);
}
