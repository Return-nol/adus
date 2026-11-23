use adus_core::{job::Trigger, types::ConnectionKind};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

use crate::{Result, repositories};

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

/// Hapus baris review (engine: percobaan ulang akan membuat baris baru).
pub async fn delete(pool: &PgPool, id: Uuid) -> Result<()> {
    sqlx::query("DELETE FROM reviews WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Satu baris `failed` yang langsung selesai (worker: job review menyerah setelah semua
/// percobaan). Tidak menulis apa pun jika repo-nya sudah tidak ada atau dihapus; return
/// `true` jika baris dibuat.
pub async fn insert_failed(pool: &PgPool, new: NewReview, error: &str) -> Result<bool> {
    let inserted = sqlx::query(
        "INSERT INTO reviews (repository_id, pr_number, trigger, status, error, started_at, \
         finished_at) \
         SELECT id, $2, $3, 'failed', $4, now(), now() FROM repositories \
         WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(new.repository_id)
    .bind(new.pr_number)
    .bind(new.trigger)
    .bind(error)
    .execute(pool)
    .await?;
    Ok(inserted.rows_affected() == 1)
}

/// Review `queued`/`running` tanpa kemajuan selama ini dianggap tidak dikerjakan siapa pun.
/// Harus lebih lama dari `JOB_TIMEOUT` worker (dicek saat kompilasi di `adus::worker`).
pub const STALE_AFTER: Duration = Duration::from_secs(35 * 60);

/// Saat proses mulai dan berkala dari scheduler: review `queued`/`running` yang sudah lebih lama
/// dari `STALE_AFTER` ditandai gagal. Return jumlah barisnya.
pub async fn fail_stale(pool: &PgPool) -> Result<u64> {
    let updated = sqlx::query(
        "UPDATE reviews SET status = 'failed', error = $2, finished_at = now()          WHERE status IN ('queued', 'running')          AND COALESCE(started_at, created_at) < now() - make_interval(secs => $1)",
    )
    .bind(STALE_AFTER.as_secs_f64())
    .bind(format!(
        "interrupted (no progress for {} min)",
        STALE_AFTER.as_secs() / 60
    ))
    .execute(pool)
    .await?;
    Ok(updated.rows_affected())
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

/// Satu baris riwayat untuk dashboard: review + repo (juga yang soft-deleted) + total token.
#[derive(Debug, Clone, FromRow)]
pub struct HistoryRow {
    pub id: Uuid,
    pub repository_id: Uuid,
    pub repo_owner: String,
    /// Azure: GUID project. GitHub: NULL.
    pub repo_project: Option<String>,
    pub repo_name: String,
    /// Label untuk pengguna (`repositories::display_name_sql`): Azure `project/repo`.
    pub repo_display_name: String,
    pub connection_kind: ConnectionKind,
    pub pr_number: i64,
    pub trigger: Trigger,
    pub status: ReviewStatus,
    pub error: Option<String>,
    pub skip_reason: Option<String>,
    pub head_sha: Option<String>,
    pub comments_posted: i32,
    /// SUM `llm_usage` milik review ini; 0 jika tidak ada panggilan.
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
}

/// Filter `history`; `None` = semua.
pub struct HistoryFilter {
    pub repository_id: Option<Uuid>,
    pub status: Option<ReviewStatus>,
}

const HISTORY_WHERE: &str = "WHERE ($1::uuid IS NULL OR r.repository_id = $1) AND ($2::review_status IS NULL OR r.status = $2)";

/// Satu halaman riwayat (terbaru dulu: `started_at`, atau `created_at` jika belum mulai) dan jumlah
/// total baris.
/// `page` mulai dari 1. Token dijumlah dalam satu query (lateral subquery), bukan per baris.
pub async fn history(
    pool: &PgPool,
    filter: &HistoryFilter,
    page: i64,
    per_page: i64,
) -> Result<(Vec<HistoryRow>, i64)> {
    let rows = sqlx::query_as(&format!(
        "SELECT r.id, r.repository_id, p.owner AS repo_owner, p.project AS repo_project, \
         p.name AS repo_name, {display_name} AS repo_display_name, c.kind AS connection_kind, \
         r.pr_number, r.trigger, r.status, \
         r.error, r.skip_reason, r.head_sha, r.comments_posted, \
         COALESCE(u.input_tokens, 0) AS input_tokens, COALESCE(u.output_tokens, 0) AS output_tokens, \
         r.started_at, r.finished_at \
         FROM reviews r \
         JOIN repositories p ON p.id = r.repository_id \
         JOIN connections c ON c.id = p.connection_id \
         LEFT JOIN LATERAL ( \
             SELECT SUM(input_tokens)::bigint AS input_tokens, \
                    SUM(output_tokens)::bigint AS output_tokens \
             FROM llm_usage WHERE review_id = r.id \
         ) u ON true \
         {HISTORY_WHERE} \
         ORDER BY COALESCE(r.started_at, r.created_at) DESC, r.id DESC LIMIT $3 OFFSET $4",
        display_name = repositories::display_name_sql("p"),
    ))
    .bind(filter.repository_id)
    .bind(filter.status)
    .bind(per_page)
    .bind((page.max(1) - 1) * per_page)
    .fetch_all(pool)
    .await?;
    let total = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM reviews r {HISTORY_WHERE}"))
        .bind(filter.repository_id)
        .bind(filter.status)
        .fetch_one(pool)
        .await?;
    Ok((rows, total))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{connections, repositories};

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

    #[sqlx::test(migrations = "../../migrations")]
    async fn delete_and_insert_failed(pool: PgPool) {
        let a = repo(&pool, "1").await;
        let row = insert(&pool, new(a)).await.unwrap();
        delete(&pool, row.id).await.unwrap();
        assert!(list(&pool, None, 1, 10).await.unwrap().is_empty());

        assert!(
            insert_failed(&pool, new(a), "boom (after 5 attempts)")
                .await
                .unwrap()
        );
        let rows = list(&pool, None, 1, 10).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, ReviewStatus::Failed);
        assert_eq!(rows[0].trigger, Trigger::Updated);
        assert_eq!(rows[0].error.as_deref(), Some("boom (after 5 attempts)"));
        assert!(rows[0].started_at.is_some() && rows[0].finished_at.is_some());

        // Repo yang sudah dihapus atau tidak pernah ada: tidak ada baris.
        repositories::soft_delete(&pool, a).await.unwrap();
        assert!(!insert_failed(&pool, new(a), "x").await.unwrap());
        assert!(
            !insert_failed(&pool, new(Uuid::new_v4()), "x")
                .await
                .unwrap()
        );
        assert_eq!(list(&pool, None, 1, 10).await.unwrap().len(), 1);
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn fail_stale_marks_only_old_unfinished_rows(pool: PgPool) {
        let a = repo(&pool, "1").await;
        let old_queued = insert(&pool, new(a)).await.unwrap();
        let old_running = insert(&pool, new(a)).await.unwrap();
        set_running(&pool, old_running.id, "b", "h").await.unwrap();
        let old_done = insert(&pool, new(a)).await.unwrap();
        finish(&pool, old_done.id, ReviewStatus::Done, 0, None, None)
            .await
            .unwrap();
        sqlx::query(
            "UPDATE reviews SET created_at = now() - interval '40 minutes', \
             started_at = started_at - interval '40 minutes'",
        )
        .execute(&pool)
        .await
        .unwrap();
        let fresh_queued = insert(&pool, new(a)).await.unwrap();
        let fresh_running = insert(&pool, new(a)).await.unwrap();
        set_running(&pool, fresh_running.id, "b", "h")
            .await
            .unwrap();
        // Dibuat lama, tetapi baru mulai jalan: `started_at` yang dihitung.
        let restarted = insert(&pool, new(a)).await.unwrap();
        sqlx::query("UPDATE reviews SET created_at = now() - interval '40 minutes' WHERE id = $1")
            .bind(restarted.id)
            .execute(&pool)
            .await
            .unwrap();
        set_running(&pool, restarted.id, "b", "h").await.unwrap();

        assert_eq!(fail_stale(&pool).await.unwrap(), 2);

        let rows = list(&pool, None, 1, 10).await.unwrap();
        let status = |id: Uuid| rows.iter().find(|r| r.id == id).unwrap().clone();
        for id in [old_queued.id, old_running.id] {
            let row = status(id);
            assert_eq!(row.status, ReviewStatus::Failed);
            assert_eq!(
                row.error.as_deref(),
                Some("interrupted (no progress for 35 min)")
            );
            assert!(row.finished_at.is_some());
        }
        assert_eq!(status(old_done.id).status, ReviewStatus::Done);
        assert_eq!(status(fresh_queued.id).status, ReviewStatus::Queued);
        assert_eq!(status(fresh_running.id).status, ReviewStatus::Running);
        assert_eq!(status(restarted.id).status, ReviewStatus::Running);
    }
}
