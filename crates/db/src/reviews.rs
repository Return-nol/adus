use adus_core::job::Trigger;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

use crate::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "review_status", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum ReviewStatus {
    Queued,
    Running,
    Done,
    Failed,
    Skipped,
}

#[derive(Debug, Clone, FromRow)]
pub struct Review {
    pub id: Uuid,
    pub repository_id: Uuid,
    pub pr_number: i64,
    pub base_sha: Option<String>,
    pub head_sha: Option<String>,
    pub trigger: Trigger,
    pub status: ReviewStatus,
    pub error: Option<String>,
    pub skip_reason: Option<String>,
    pub comments_posted: i32,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

pub struct NewReview {
    pub repository_id: Uuid,
    pub pr_number: i64,
    pub trigger: Trigger,
}

const COLUMNS: &str = "id, repository_id, pr_number, base_sha, head_sha, trigger, status, error, \
                       skip_reason, comments_posted, started_at, finished_at, created_at";

/// Baris baru berstatus `queued`.
pub async fn insert(pool: &PgPool, new: NewReview) -> Result<Review> {
    sqlx::query_as(&format!(
        "INSERT INTO reviews (repository_id, pr_number, trigger) VALUES ($1, $2, $3) \
         RETURNING {COLUMNS}"
    ))
    .bind(new.repository_id)
    .bind(new.pr_number)
    .bind(new.trigger)
    .fetch_one(pool)
    .await
}

/// PR sudah diambil: catat commit yang direview dan mulai `running`.
pub async fn set_running(pool: &PgPool, id: Uuid, base_sha: &str, head_sha: &str) -> Result<()> {
    sqlx::query(
        "UPDATE reviews SET status = 'running', base_sha = $2, head_sha = $3, \
         started_at = now() WHERE id = $1",
    )
    .bind(id)
    .bind(base_sha)
    .bind(head_sha)
    .execute(pool)
    .await?;
    Ok(())
}

/// Status akhir (`done` | `failed` | `skipped`).
pub async fn finish(
    pool: &PgPool,
    id: Uuid,
    status: ReviewStatus,
    comments_posted: i32,
    error: Option<&str>,
    skip_reason: Option<&str>,
) -> Result<()> {
    sqlx::query(
        "UPDATE reviews SET status = $2, comments_posted = $3, error = $4, skip_reason = $5, \
         finished_at = now() WHERE id = $1",
    )
    .bind(id)
    .bind(status)
    .bind(comments_posted)
    .bind(error)
    .bind(skip_reason)
    .execute(pool)
    .await?;
    Ok(())
}

/// Review `done` terakhir untuk PR ini (dasar review inkremental).
pub async fn last_done(
    pool: &PgPool,
    repository_id: Uuid,
    pr_number: i64,
) -> Result<Option<Review>> {
    sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM reviews \
         WHERE repository_id = $1 AND pr_number = $2 AND status = 'done' \
         ORDER BY finished_at DESC, created_at DESC LIMIT 1"
    ))
    .bind(repository_id)
    .bind(pr_number)
    .fetch_optional(pool)
    .await
}

/// Terbaru dulu. `page` mulai dari 1.
pub async fn list(
    pool: &PgPool,
    repository_id: Option<Uuid>,
    page: i64,
    per_page: i64,
) -> Result<Vec<Review>> {
    sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM reviews WHERE ($1::uuid IS NULL OR repository_id = $1) \
         ORDER BY created_at DESC, id LIMIT $2 OFFSET $3"
    ))
    .bind(repository_id)
    .bind(per_page)
    .bind((page.max(1) - 1) * per_page)
    .fetch_all(pool)
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{connections, repositories};
    use adus_core::types::ConnectionKind;

    async fn repo(pool: &PgPool, external_id: &str) -> Uuid {
        let conn =
            connections::insert(pool, ConnectionKind::Github, "c", "acme", "e", "1234", "me")
                .await
                .unwrap();
        repositories::insert(
            pool,
            repositories::NewRepository {
                connection_id: conn.id,
                external_id,
                owner: "acme",
                project: None,
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

    fn new(repository_id: Uuid) -> NewReview {
        NewReview {
            repository_id,
            pr_number: 7,
            trigger: Trigger::Updated,
        }
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn lifecycle_last_done_and_list(pool: PgPool) {
        let a = repo(&pool, "1").await;
        let b = repo(&pool, "2").await;

        let first = insert(&pool, new(a)).await.unwrap();
        assert_eq!(first.status, ReviewStatus::Queued);
        assert_eq!(first.trigger, Trigger::Updated);
        set_running(&pool, first.id, "b1", "h1").await.unwrap();
        finish(&pool, first.id, ReviewStatus::Done, 3, None, None)
            .await
            .unwrap();

        let failed = insert(&pool, new(a)).await.unwrap();
        finish(
            &pool,
            failed.id,
            ReviewStatus::Failed,
            0,
            Some("boom"),
            None,
        )
        .await
        .unwrap();
        insert(&pool, new(b)).await.unwrap();

        let last = last_done(&pool, a, 7).await.unwrap().unwrap();
        assert_eq!(last.id, first.id);
        assert_eq!(last.head_sha.as_deref(), Some("h1"));
        assert_eq!(last.base_sha.as_deref(), Some("b1"));
        assert_eq!(last.comments_posted, 3);
        assert!(last.started_at.is_some() && last.finished_at.is_some());
        assert!(last_done(&pool, a, 8).await.unwrap().is_none());
        assert!(last_done(&pool, b, 7).await.unwrap().is_none());

        assert_eq!(list(&pool, None, 1, 20).await.unwrap().len(), 3);
        let page1 = list(&pool, Some(a), 1, 1).await.unwrap();
        let page2 = list(&pool, Some(a), 2, 1).await.unwrap();
        assert_eq!(page1[0].id, failed.id, "newest first");
        assert_eq!(page1[0].error.as_deref(), Some("boom"));
        assert_eq!(page2[0].id, first.id);
    }
}
