use adus_core::types::ConnectionKind;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

use crate::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "connection_status", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum ConnectionStatus {
    Ok,
    Invalid,
    Unverified,
}

#[derive(Debug, Clone, FromRow)]
pub struct Connection {
    pub id: Uuid,
    pub kind: ConnectionKind,
    pub name: String,
    pub org_or_owner: String,
    pub encrypted_token: String,
    pub token_last4: String,
    pub status: ConnectionStatus,
    pub pat_owner: Option<String>,
    pub last_verified_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

/// Hasil `delete`.
#[derive(Debug, PartialEq, Eq)]
pub enum DeleteOutcome {
    Deleted,
    NotFound,
    /// Masih ada repo aktif; tidak ada yang dihapus.
    InUse,
}

const COLUMNS: &str = "id, kind, name, org_or_owner, encrypted_token, token_last4, status, \
                       pat_owner, last_verified_at, created_at";

/// Dipanggil setelah token terverifikasi, jadi status langsung `ok`.
pub async fn insert(
    pool: &PgPool,
    kind: ConnectionKind,
    name: &str,
    org_or_owner: &str,
    encrypted_token: &str,
    token_last4: &str,
    pat_owner: &str,
) -> Result<Connection> {
    sqlx::query_as(&format!(
        "INSERT INTO connections \
         (kind, name, org_or_owner, encrypted_token, token_last4, status, pat_owner, last_verified_at) \
         VALUES ($1, $2, $3, $4, $5, 'ok', $6, now()) RETURNING {COLUMNS}"
    ))
    .bind(kind)
    .bind(name)
    .bind(org_or_owner)
    .bind(encrypted_token)
    .bind(token_last4)
    .bind(pat_owner)
    .fetch_one(pool)
    .await
}

pub async fn list(pool: &PgPool) -> Result<Vec<Connection>> {
    sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM connections ORDER BY created_at, id"
    ))
    .fetch_all(pool)
    .await
}

pub async fn get(pool: &PgPool, id: Uuid) -> Result<Option<Connection>> {
    sqlx::query_as(&format!("SELECT {COLUMNS} FROM connections WHERE id = $1"))
        .bind(id)
        .fetch_optional(pool)
        .await
}

/// Ganti token (mis. rotasi PAT). Hanya menyentuh kolom token; tabel `repositories` tidak disentuh.
pub async fn update_token(
    pool: &PgPool,
    id: Uuid,
    encrypted_token: &str,
    token_last4: &str,
    pat_owner: &str,
    status: ConnectionStatus,
) -> Result<Option<Connection>> {
    sqlx::query_as(&format!(
        "UPDATE connections SET encrypted_token = $2, token_last4 = $3, pat_owner = $4, \
         status = $5, last_verified_at = now() WHERE id = $1 RETURNING {COLUMNS}"
    ))
    .bind(id)
    .bind(encrypted_token)
    .bind(token_last4)
    .bind(pat_owner)
    .bind(status)
    .fetch_optional(pool)
    .await
}

pub async fn set_status(pool: &PgPool, id: Uuid, status: ConnectionStatus) -> Result<()> {
    sqlx::query("UPDATE connections SET status = $2 WHERE id = $1")
        .bind(id)
        .bind(status)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn update_name(pool: &PgPool, id: Uuid, name: &str) -> Result<Option<Connection>> {
    sqlx::query_as(&format!(
        "UPDATE connections SET name = $2 WHERE id = $1 RETURNING {COLUMNS}"
    ))
    .bind(id)
    .bind(name)
    .fetch_optional(pool)
    .await
}

/// Hapus connection hanya jika tidak ada repo aktif. Baris repo yang sudah soft-deleted ikut dibuang
/// (kalau tidak, foreign key menahan penghapusan). Baris connection dikunci selama transaksi.
pub async fn delete(pool: &PgPool, id: Uuid) -> Result<DeleteOutcome> {
    let mut tx = pool.begin().await?;
    let found: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM connections WHERE id = $1 FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
    if found.is_none() {
        return Ok(DeleteOutcome::NotFound);
    }
    let active: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM repositories WHERE connection_id = $1 AND deleted_at IS NULL",
    )
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    if active > 0 {
        return Ok(DeleteOutcome::InUse);
    }
    sqlx::query("DELETE FROM repositories WHERE connection_id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM connections WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(DeleteOutcome::Deleted)
}
