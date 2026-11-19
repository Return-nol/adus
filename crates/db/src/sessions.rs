use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::Result;
use crate::users::{COLUMNS, User};

pub async fn create(
    pool: &PgPool,
    user_id: Uuid,
    token_hash: &str,
    expires_at: DateTime<Utc>,
) -> Result<()> {
    sqlx::query("INSERT INTO sessions (token_hash, user_id, expires_at) VALUES ($1, $2, $3)")
        .bind(token_hash)
        .bind(user_id)
        .bind(expires_at)
        .execute(pool)
        .await?;
    Ok(())
}

/// User pemilik sesi, hanya jika sesi belum kedaluwarsa dan user tidak disabled.
pub async fn user_for(pool: &PgPool, token_hash: &str) -> Result<Option<User>> {
    sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM users WHERE NOT disabled AND id IN \
         (SELECT user_id FROM sessions WHERE token_hash = $1 AND expires_at > now())"
    ))
    .bind(token_hash)
    .fetch_optional(pool)
    .await
}

pub async fn delete(pool: &PgPool, token_hash: &str) -> Result<()> {
    sqlx::query("DELETE FROM sessions WHERE token_hash = $1")
        .bind(token_hash)
        .execute(pool)
        .await?;
    Ok(())
}

/// Hapus semua sesi user, kecuali sesi dengan `except_token_hash` (mis. sesi yang sedang dipakai).
pub async fn delete_for_user(
    pool: &PgPool,
    user_id: Uuid,
    except_token_hash: Option<&str>,
) -> Result<()> {
    sqlx::query("DELETE FROM sessions WHERE user_id = $1 AND token_hash IS DISTINCT FROM $2")
        .bind(user_id)
        .bind(except_token_hash)
        .execute(pool)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use chrono::Duration;

    use super::*;
    use crate::users::{self, Role, UserPatch};

    #[sqlx::test(migrations = "../../migrations")]
    async fn session_for_disabled_user_is_none(pool: PgPool) {
        let user = users::insert(&pool, "a@x.io", "h", Role::User, None)
            .await
            .unwrap();
        create(&pool, user.id, "tok", Utc::now() + Duration::days(1))
            .await
            .unwrap();
        assert!(user_for(&pool, "tok").await.unwrap().is_some());

        let patch = UserPatch {
            disabled: Some(true),
            ..Default::default()
        };
        users::update(&pool, user.id, patch).await.unwrap();
        assert!(user_for(&pool, "tok").await.unwrap().is_none());
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn expired_session_is_none(pool: PgPool) {
        let user = users::insert(&pool, "a@x.io", "h", Role::User, None)
            .await
            .unwrap();
        create(&pool, user.id, "tok", Utc::now() - Duration::seconds(1))
            .await
            .unwrap();
        assert!(user_for(&pool, "tok").await.unwrap().is_none());
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn deleted_session_is_none(pool: PgPool) {
        let user = users::insert(&pool, "a@x.io", "h", Role::User, None)
            .await
            .unwrap();
        create(&pool, user.id, "tok", Utc::now() + Duration::days(1))
            .await
            .unwrap();
        delete(&pool, "tok").await.unwrap();
        assert!(user_for(&pool, "tok").await.unwrap().is_none());
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn delete_for_user_keeps_only_the_excepted_session(pool: PgPool) {
        let ann = users::insert(&pool, "a@x.io", "h", Role::User, None)
            .await
            .unwrap();
        let bob = users::insert(&pool, "b@x.io", "h", Role::User, None)
            .await
            .unwrap();
        let expires = Utc::now() + Duration::days(1);
        for (user, token) in [(&ann, "a1"), (&ann, "a2"), (&bob, "b1")] {
            create(&pool, user.id, token, expires).await.unwrap();
        }

        delete_for_user(&pool, ann.id, Some("a1")).await.unwrap();
        assert!(user_for(&pool, "a1").await.unwrap().is_some());
        assert!(user_for(&pool, "a2").await.unwrap().is_none());
        assert!(user_for(&pool, "b1").await.unwrap().is_some());

        delete_for_user(&pool, ann.id, None).await.unwrap();
        assert!(user_for(&pool, "a1").await.unwrap().is_none());
        assert!(user_for(&pool, "b1").await.unwrap().is_some());
    }
}
