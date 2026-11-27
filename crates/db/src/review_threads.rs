//! Finding threads Adus opened on a PR, and what became of them.

use adus_core::types::Severity;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgPool, Postgres, QueryBuilder};
use uuid::Uuid;

use crate::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "thread_outcome", rename_all = "snake_case")]
pub enum ThreadOutcome {
    Open,
    Fixed,
    WontFix,
    ByDesign,
}

#[derive(Debug, Clone, FromRow)]
pub struct ReviewThread {
    pub id: Uuid,
    pub repository_id: Uuid,
    pub review_id: Option<Uuid>,
    pub pr_number: i64,
    /// Azure thread id, or the GitHub root review comment id.
    pub provider_thread_id: String,
    pub root_comment_id: String,
    pub path: String,
    pub line: Option<i32>,
    pub severity: Severity,
    pub title: String,
    pub body: String,
    pub posted_head_sha: String,
    pub outcome: ThreadOutcome,
    pub reason: Option<String>,
    pub fix_sha: Option<String>,
    pub resolution: Option<String>,
    pub adus_replies: i32,
    pub validated_head_sha: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

pub struct NewReviewThread<'a> {
    pub repository_id: Uuid,
    pub review_id: Option<Uuid>,
    pub pr_number: i64,
    pub provider_thread_id: &'a str,
    pub root_comment_id: &'a str,
    pub path: &'a str,
    pub line: Option<i32>,
    pub severity: Severity,
    pub title: &'a str,
    pub body: &'a str,
    pub posted_head_sha: &'a str,
}

pub struct ThreadUpdate<'a> {
    pub outcome: ThreadOutcome,
    pub reason: Option<&'a str>,
    pub fix_sha: Option<&'a str>,
    pub resolution: Option<&'a str>,
}

const COLUMNS: &str = "id, repository_id, review_id, pr_number, provider_thread_id, \
     root_comment_id, path, line, severity, title, body, posted_head_sha, outcome, reason, \
     fix_sha, resolution, adus_replies, validated_head_sha, created_at, updated_at";

/// A thread already recorded for the PR is left as it is.
pub async fn insert_many(pool: &PgPool, rows: &[NewReviewThread<'_>]) -> Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    let mut query = QueryBuilder::<Postgres>::new(
        "INSERT INTO review_threads (repository_id, review_id, pr_number, provider_thread_id, \
         root_comment_id, path, line, severity, title, body, posted_head_sha) ",
    );
    query.push_values(rows, |mut row, new| {
        row.push_bind(new.repository_id)
            .push_bind(new.review_id)
            .push_bind(new.pr_number)
            .push_bind(new.provider_thread_id)
            .push_bind(new.root_comment_id)
            .push_bind(new.path)
            .push_bind(new.line)
            .push_bind(new.severity)
            .push_bind(new.title)
            .push_bind(new.body)
            .push_bind(new.posted_head_sha);
    });
    query.push(" ON CONFLICT DO NOTHING");
    query.build().execute(pool).await?;
    Ok(())
}

pub async fn find(
    pool: &PgPool,
    repository_id: Uuid,
    pr_number: i64,
    provider_thread_id: &str,
) -> Result<Option<ReviewThread>> {
    sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM review_threads \
         WHERE repository_id = $1 AND pr_number = $2 AND provider_thread_id = $3"
    ))
    .bind(repository_id)
    .bind(pr_number)
    .bind(provider_thread_id)
    .fetch_optional(pool)
    .await
}

/// Threads still `open`, newest first.
pub async fn open_for_pr(
    pool: &PgPool,
    repository_id: Uuid,
    pr_number: i64,
) -> Result<Vec<ReviewThread>> {
    sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM review_threads \
         WHERE repository_id = $1 AND pr_number = $2 AND outcome = 'open' \
         ORDER BY created_at DESC, id"
    ))
    .bind(repository_id)
    .bind(pr_number)
    .fetch_all(pool)
    .await
}

/// Every thread of the PR, newest first.
pub async fn for_pr(
    pool: &PgPool,
    repository_id: Uuid,
    pr_number: i64,
) -> Result<Vec<ReviewThread>> {
    sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM review_threads \
         WHERE repository_id = $1 AND pr_number = $2 ORDER BY created_at DESC, id"
    ))
    .bind(repository_id)
    .bind(pr_number)
    .fetch_all(pool)
    .await
}

pub async fn set_outcome(pool: &PgPool, id: Uuid, update: ThreadUpdate<'_>) -> Result<()> {
    sqlx::query(
        "UPDATE review_threads SET outcome = $2, reason = $3, fix_sha = $4, resolution = $5, \
         updated_at = now() WHERE id = $1",
    )
    .bind(id)
    .bind(update.outcome)
    .bind(update.reason)
    .bind(update.fix_sha)
    .bind(update.resolution)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn record_reply(pool: &PgPool, id: Uuid) -> Result<()> {
    sqlx::query(
        "UPDATE review_threads SET adus_replies = adus_replies + 1, updated_at = now() \
         WHERE id = $1",
    )
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn set_validated(pool: &PgPool, id: Uuid, head_sha: &str) -> Result<()> {
    sqlx::query(
        "UPDATE review_threads SET validated_head_sha = $2, updated_at = now() WHERE id = $1",
    )
    .bind(id)
    .bind(head_sha)
    .execute(pool)
    .await?;
    Ok(())
}
