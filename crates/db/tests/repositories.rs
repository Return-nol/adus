use adus_core::types::{ConnectionKind, LearningMode, ReviewMode};
use adus_db::connections;
use adus_db::repositories::{self, NewRepository, RepoPatch};
use sqlx::PgPool;

#[sqlx::test(migrations = "../../migrations")]
async fn repository_review_and_learning_mode_set_and_clear(pool: PgPool) {
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
    .unwrap();
    assert_eq!(repo.review_mode, None);
    assert_eq!(repo.learning_mode, LearningMode::Approval);

    let patch = RepoPatch {
        review_mode: Some(Some(ReviewMode::Specialists)),
        learning_mode: Some(LearningMode::Auto),
        ..Default::default()
    };
    let set = repositories::update(&pool, repo.id, patch)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(set.review_mode, Some(ReviewMode::Specialists));
    assert_eq!(set.learning_mode, LearningMode::Auto);

    // Patching another column leaves review_mode and learning_mode alone.
    let patch = RepoPatch {
        enabled: Some(false),
        ..Default::default()
    };
    let kept = repositories::update(&pool, repo.id, patch)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(kept.review_mode, Some(ReviewMode::Specialists));
    assert_eq!(kept.learning_mode, LearningMode::Auto);

    let patch = RepoPatch {
        review_mode: Some(None),
        ..Default::default()
    };
    let cleared = repositories::update(&pool, repo.id, patch)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(cleared.review_mode, None);
}
