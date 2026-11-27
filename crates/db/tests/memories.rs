use adus_core::types::ConnectionKind;
use adus_db::{
    connections,
    memories::{self, Evidence, Memory, MemoryKind, MemorySource, MemoryStatus, NewMemory},
    repositories::{self, NewRepository},
    rules::KnowledgeScope,
};
use sqlx::PgPool;
use uuid::Uuid;

async fn add_repo(pool: &PgPool) -> Uuid {
    let conn = connections::insert(
        pool,
        ConnectionKind::Github,
        "c",
        "acme",
        "enc",
        "1234",
        "octocat",
    )
    .await
    .unwrap();
    repositories::insert(
        pool,
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
    .id
}

async fn add(pool: &PgPool, repo: Uuid, content: &str, status: MemoryStatus) -> Memory {
    add_superseding(pool, repo, content, status, None, &[]).await
}

async fn add_superseding(
    pool: &PgPool,
    repo: Uuid,
    content: &str,
    status: MemoryStatus,
    supersedes: Option<Uuid>,
    evidence: &[Evidence],
) -> Memory {
    memories::insert(
        pool,
        NewMemory {
            scope: KnowledgeScope::Repo,
            repository_id: Some(repo),
            content,
            source: MemorySource::PrLearning,
            source_url: None,
            created_by: None,
            status,
            kind: MemoryKind::Convention,
            context: None,
            reason: None,
            resolution: None,
            evidence,
            supersedes,
        },
    )
    .await
    .unwrap()
}

fn contents(memories: &[Memory]) -> Vec<&str> {
    memories.iter().map(|m| m.content.as_str()).collect()
}

#[sqlx::test(migrations = "../../migrations")]
async fn applicable_excludes_pending_and_rejected(pool: PgPool) {
    let repo = add_repo(&pool).await;
    add(&pool, repo, "active", MemoryStatus::Active).await;
    add(&pool, repo, "pending", MemoryStatus::Pending).await;
    add(&pool, repo, "rejected", MemoryStatus::Rejected).await;

    let got = memories::applicable(&pool, repo).await.unwrap();
    assert_eq!(contents(&got), ["active"]);

    // Learning sees every status.
    let got = memories::for_learning(&pool, repo).await.unwrap();
    assert_eq!(contents(&got), ["active", "pending", "rejected"]);
}

#[sqlx::test(migrations = "../../migrations")]
async fn approve_and_reject_only_from_pending(pool: PgPool) {
    let repo = add_repo(&pool).await;
    let first = add(&pool, repo, "old", MemoryStatus::Pending).await;

    let approved = memories::approve(&pool, first.id, Some("new"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(approved.status, MemoryStatus::Active);
    assert_eq!(approved.content, "new");
    assert!(
        memories::approve(&pool, first.id, None)
            .await
            .unwrap()
            .is_none()
    );
    assert!(memories::reject(&pool, first.id).await.unwrap().is_none());

    let second = add(&pool, repo, "x", MemoryStatus::Pending).await;
    let rejected = memories::reject(&pool, second.id).await.unwrap().unwrap();
    assert_eq!(rejected.status, MemoryStatus::Rejected);
    assert_eq!(rejected.content, "x");
    let got = memories::get(&pool, second.id).await.unwrap().unwrap();
    assert_eq!(got.status, MemoryStatus::Rejected);
}

#[sqlx::test(migrations = "../../migrations")]
async fn list_hides_rejected_unless_asked(pool: PgPool) {
    let repo = add_repo(&pool).await;
    add(&pool, repo, "active", MemoryStatus::Active).await;
    add(&pool, repo, "pending", MemoryStatus::Pending).await;
    add(&pool, repo, "rejected", MemoryStatus::Rejected).await;

    let got = memories::list(&pool, Some(repo), None).await.unwrap();
    assert_eq!(contents(&got), ["active", "pending"]);
    let got = memories::list(&pool, Some(repo), Some(MemoryStatus::Rejected))
        .await
        .unwrap();
    assert_eq!(contents(&got), ["rejected"]);
    let got = memories::list_all(&pool, None).await.unwrap();
    assert_eq!(contents(&got), ["active", "pending"]);
    let got = memories::list_all(&pool, Some(MemoryStatus::Pending))
        .await
        .unwrap();
    assert_eq!(contents(&got), ["pending"]);
}

#[sqlx::test(migrations = "../../migrations")]
async fn superseded_memory_leaves_prompts(pool: PgPool) {
    let repo = add_repo(&pool).await;
    let a = add(&pool, repo, "A", MemoryStatus::Active).await;
    let evidence = [Evidence {
        url: "https://example.com/pr/7".into(),
        pr_number: Some(7),
    }];
    let b = add_superseding(
        &pool,
        repo,
        "B",
        MemoryStatus::Pending,
        Some(a.id),
        &evidence,
    )
    .await;
    assert_eq!(b.supersedes, Some(a.id));
    let stored = memories::get(&pool, b.id).await.unwrap().unwrap();
    assert_eq!(stored.evidence, evidence);
    let got = memories::applicable(&pool, repo).await.unwrap();
    assert_eq!(contents(&got), ["A"]);

    memories::approve(&pool, b.id, None).await.unwrap().unwrap();
    let got = memories::applicable(&pool, repo).await.unwrap();
    assert_eq!(contents(&got), ["B"]);
    let a = memories::get(&pool, a.id).await.unwrap().unwrap();
    assert_eq!(a.superseded_by, Some(b.id));

    // Rejecting a replacement leaves the memory it targets in use.
    let c = add_superseding(&pool, repo, "C", MemoryStatus::Pending, Some(b.id), &[]).await;
    memories::reject(&pool, c.id).await.unwrap().unwrap();
    let got = memories::applicable(&pool, repo).await.unwrap();
    assert_eq!(contents(&got), ["B"]);

    // Auto mode: inserting the replacement active retires its target in the same statement.
    let d = add_superseding(&pool, repo, "D", MemoryStatus::Active, Some(b.id), &[]).await;
    let got = memories::applicable(&pool, repo).await.unwrap();
    assert_eq!(contents(&got), ["D"]);

    // An already-superseded target keeps its first replacement.
    add_superseding(&pool, repo, "E", MemoryStatus::Active, Some(b.id), &[]).await;
    let b = memories::get(&pool, b.id).await.unwrap().unwrap();
    assert_eq!(b.superseded_by, Some(d.id));
}
