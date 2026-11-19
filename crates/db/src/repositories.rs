use adus_core::types::ConnectionKind;
use chrono::{DateTime, Utc};
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

use crate::Result;

#[derive(Debug, Clone, FromRow)]
pub struct Repository {
    pub id: Uuid,
    pub connection_id: Uuid,
    pub external_id: String,
    pub owner: String,
    pub project: Option<String>,
    pub name: String,
    pub default_branch: Option<String>,
    pub enabled: bool,
    pub auto_review: bool,
    pub webhook_external_ids: Vec<String>,
    pub webhook_token: Option<String>,
    pub review_provider_id: Option<Uuid>,
    pub review_model: Option<String>,
    pub created_at: DateTime<Utc>,
}

pub struct NewRepository<'a> {
    pub connection_id: Uuid,
    pub external_id: &'a str,
    pub owner: &'a str,
    pub project: Option<&'a str>,
    pub name: &'a str,
    pub default_branch: Option<&'a str>,
    pub webhook_token: &'a str,
}

/// Perubahan parsial. `None` = biarkan; untuk dua kolom nullable, `Some(None)` = kosongkan.
#[derive(Debug, Default)]
pub struct RepoPatch {
    pub enabled: Option<bool>,
    pub auto_review: Option<bool>,
    pub review_provider_id: Option<Option<Uuid>>,
    pub review_model: Option<Option<String>>,
}

// Semua query di bawah hanya melihat repo aktif (`deleted_at IS NULL`).
const COLUMNS: &str = "id, connection_id, external_id, owner, project, name, default_branch, \
                       enabled, auto_review, webhook_external_ids, webhook_token, \
                       review_provider_id, review_model, created_at";

/// `None` jika repo yang sama sudah aktif di connection ini.
pub async fn insert(pool: &PgPool, new: NewRepository<'_>) -> Result<Option<Repository>> {
    sqlx::query_as(&format!(
        "INSERT INTO repositories \
         (connection_id, external_id, owner, project, name, default_branch, webhook_token) \
         VALUES ($1, $2, $3, $4, $5, $6, $7) \
         ON CONFLICT (connection_id, external_id) WHERE deleted_at IS NULL DO NOTHING \
         RETURNING {COLUMNS}"
    ))
    .bind(new.connection_id)
    .bind(new.external_id)
    .bind(new.owner)
    .bind(new.project)
    .bind(new.name)
    .bind(new.default_branch)
    .bind(new.webhook_token)
    .fetch_optional(pool)
    .await
}

pub async fn list(pool: &PgPool, connection_id: Option<Uuid>) -> Result<Vec<Repository>> {
    sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM repositories \
         WHERE deleted_at IS NULL AND ($1::uuid IS NULL OR connection_id = $1) \
         ORDER BY created_at, id"
    ))
    .bind(connection_id)
    .fetch_all(pool)
    .await
}

pub async fn get(pool: &PgPool, id: Uuid) -> Result<Option<Repository>> {
    sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM repositories WHERE id = $1 AND deleted_at IS NULL"
    ))
    .bind(id)
    .fetch_optional(pool)
    .await
}

pub async fn update(pool: &PgPool, id: Uuid, patch: RepoPatch) -> Result<Option<Repository>> {
    // $4 / $6 menandai apakah kolom nullable diubah (termasuk dikosongkan); $5 / $7 nilai barunya.
    sqlx::query_as(&format!(
        "UPDATE repositories SET \
           enabled = COALESCE($2, enabled), \
           auto_review = COALESCE($3, auto_review), \
           review_provider_id = CASE WHEN $4 THEN $5 ELSE review_provider_id END, \
           review_model = CASE WHEN $6 THEN $7 ELSE review_model END \
         WHERE id = $1 AND deleted_at IS NULL RETURNING {COLUMNS}"
    ))
    .bind(id)
    .bind(patch.enabled)
    .bind(patch.auto_review)
    .bind(patch.review_provider_id.is_some())
    .bind(patch.review_provider_id.flatten())
    .bind(patch.review_model.is_some())
    .bind(patch.review_model.flatten())
    .fetch_optional(pool)
    .await
}

pub async fn set_webhook_ids(pool: &PgPool, id: Uuid, ids: &[String]) -> Result<()> {
    sqlx::query("UPDATE repositories SET webhook_external_ids = $2 WHERE id = $1")
        .bind(id)
        .bind(ids)
        .execute(pool)
        .await?;
    Ok(())
}

/// Soft delete; `false` jika repo tidak ada atau sudah terhapus.
pub async fn soft_delete(pool: &PgPool, id: Uuid) -> Result<bool> {
    let done = sqlx::query(
        "UPDATE repositories SET deleted_at = now() WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(id)
    .execute(pool)
    .await?;
    Ok(done.rows_affected() > 0)
}

/// Hapus baris sungguhan. Hanya untuk repo yang baru di-insert lalu gagal mendaftarkan webhook.
pub async fn discard(pool: &PgPool, id: Uuid) -> Result<()> {
    sqlx::query("DELETE FROM repositories WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn active_count_for_connection(pool: &PgPool, connection_id: Uuid) -> Result<i64> {
    sqlx::query_scalar(
        "SELECT count(*) FROM repositories WHERE connection_id = $1 AND deleted_at IS NULL",
    )
    .bind(connection_id)
    .fetch_one(pool)
    .await
}

/// URL webhook yang didaftarkan di provider. Azure membawa token di query string karena
/// Azure tidak menandatangani payload; GitHub memakai HMAC dengan `webhook_token` sebagai secret.
pub fn webhook_url(public_url: &str, repo: &Repository, kind: ConnectionKind) -> String {
    match kind {
        ConnectionKind::Github => format!("{public_url}/webhooks/github/{}", repo.id),
        ConnectionKind::AzureDevops => format!(
            "{public_url}/webhooks/azure/{}?token={}",
            repo.id,
            repo.webhook_token.as_deref().unwrap_or_default()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connections;

    async fn connection(pool: &PgPool) -> Uuid {
        connections::insert(
            pool,
            ConnectionKind::Github,
            "c",
            "acme",
            "enc",
            "1234",
            "me",
        )
        .await
        .unwrap()
        .id
    }

    fn new_repo(connection_id: Uuid) -> NewRepository<'static> {
        NewRepository {
            connection_id,
            external_id: "7",
            owner: "acme",
            project: None,
            name: "api",
            default_branch: Some("main"),
            webhook_token: "t",
        }
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn duplicate_active_insert_returns_none_but_soft_deleted_does_not_block(pool: PgPool) {
        let conn = connection(&pool).await;
        let first = insert(&pool, new_repo(conn)).await.unwrap().unwrap();
        assert!(insert(&pool, new_repo(conn)).await.unwrap().is_none());

        assert!(soft_delete(&pool, first.id).await.unwrap());
        assert!(get(&pool, first.id).await.unwrap().is_none());
        let second = insert(&pool, new_repo(conn)).await.unwrap().unwrap();
        assert_ne!(second.id, first.id);
        assert_eq!(active_count_for_connection(&pool, conn).await.unwrap(), 1);
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn update_token_leaves_repositories_untouched(pool: PgPool) {
        let conn = connection(&pool).await;
        insert(&pool, new_repo(conn)).await.unwrap().unwrap();

        let updated = connections::update_token(
            &pool,
            conn,
            "enc2",
            "5678",
            "other",
            connections::ConnectionStatus::Ok,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(updated.pat_owner.as_deref(), Some("other"));
        assert_eq!(list(&pool, Some(conn)).await.unwrap().len(), 1);
    }

    #[test]
    fn webhook_url_per_kind() {
        let repo = Repository {
            id: Uuid::nil(),
            connection_id: Uuid::nil(),
            external_id: "1".into(),
            owner: "o".into(),
            project: None,
            name: "n".into(),
            default_branch: None,
            enabled: true,
            auto_review: true,
            webhook_external_ids: vec![],
            webhook_token: Some("tok".into()),
            review_provider_id: None,
            review_model: None,
            created_at: Utc::now(),
        };
        let id = Uuid::nil();
        assert_eq!(
            webhook_url("https://a.b", &repo, ConnectionKind::Github),
            format!("https://a.b/webhooks/github/{id}")
        );
        assert_eq!(
            webhook_url("https://a.b", &repo, ConnectionKind::AzureDevops),
            format!("https://a.b/webhooks/azure/{id}?token=tok")
        );
    }
}
