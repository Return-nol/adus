use adus_core::types::ConnectionKind;
use adus_db::{
    connections,
    learning_runs::{self, LearningOutcome, NewLearningRun},
    repositories::{self, NewRepository},
};
use sqlx::PgPool;

#[sqlx::test(migrations = "../../migrations")]
async fn learning_run_record_is_idempotent(pool: PgPool) {
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

    assert!(
        !learning_runs::exists(&pool, repo.id, 7, "abc")
            .await
            .unwrap()
    );
    for outcome in [LearningOutcome::Learned, LearningOutcome::Failed] {
        let run = NewLearningRun {
            repository_id: repo.id,
            pr_number: 7,
            head_sha: "abc",
            outcome,
            learned_count: 2,
        };
        learning_runs::record(&pool, run).await.unwrap();
    }
    let (count, outcome): (i64, String) = sqlx::query_as(
        "SELECT count(*), min(outcome) FROM pr_learning_runs WHERE repository_id = $1",
    )
    .bind(repo.id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((count, outcome.as_str()), (1, "learned"));
    assert!(
        learning_runs::exists(&pool, repo.id, 7, "abc")
            .await
            .unwrap()
    );
    assert!(
        !learning_runs::exists(&pool, repo.id, 7, "def")
            .await
            .unwrap()
    );
}
