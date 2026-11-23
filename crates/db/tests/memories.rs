use adus_core::types::ConnectionKind;
use adus_db::{
    connections,
    memories::{self, Memory, MemorySource, MemoryStatus, NewMemory},
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
