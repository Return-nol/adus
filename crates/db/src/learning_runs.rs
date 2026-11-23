//! Learning runs: one row per closed PR head, so a repeated close event is learned only once.

use sqlx::PgPool;
use uuid::Uuid;

use crate::Result;

/// Disimpan sebagai teks di `pr_learning_runs.outcome`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LearningOutcome {
    Learned,
    NothingToLearn,
    Skipped,
    Failed,
}

impl LearningOutcome {
    fn as_str(self) -> &'static str {
        match self {
            LearningOutcome::Learned => "learned",
            LearningOutcome::NothingToLearn => "nothing_to_learn",
            LearningOutcome::Skipped => "skipped",
            LearningOutcome::Failed => "failed",
        }
    }
}

pub struct NewLearningRun<'a> {
    pub repository_id: Uuid,
    pub pr_number: i64,
    pub head_sha: &'a str,
    pub outcome: LearningOutcome,
    pub learned_count: i32,
}

/// `true` jika head PR ini sudah pernah diproses.
pub async fn exists(
    pool: &PgPool,
    repository_id: Uuid,
    pr_number: i64,
    head_sha: &str,
) -> Result<bool> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pr_learning_runs \
         WHERE repository_id = $1 AND pr_number = $2 AND head_sha = $3)",
    )
    .bind(repository_id)
    .bind(pr_number)
    .bind(head_sha)
    .fetch_one(pool)
    .await
}

/// Catat satu run; run kedua untuk head yang sama diabaikan (baris pertama tetap).
pub async fn record(pool: &PgPool, run: NewLearningRun<'_>) -> Result<()> {
    sqlx::query(
        "INSERT INTO pr_learning_runs (repository_id, pr_number, head_sha, outcome, learned_count) \
         VALUES ($1, $2, $3, $4, $5) \
         ON CONFLICT (repository_id, pr_number, head_sha) DO NOTHING",
    )
    .bind(run.repository_id)
    .bind(run.pr_number)
    .bind(run.head_sha)
    .bind(run.outcome.as_str())
    .bind(run.learned_count)
    .execute(pool)
    .await?;
    Ok(())
}
