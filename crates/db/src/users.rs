use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgConnection, PgPool};
use uuid::Uuid;

use crate::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "user_role", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Admin,
    User,
}

#[derive(Debug, Clone, FromRow)]
pub struct User {
    pub id: Uuid,
    pub email: String,
    pub password_hash: String,
    pub role: Role,
    pub disabled: bool,
    pub github_login: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// Perubahan parsial. `github_login`: `None` = biarkan, `Some(None)` = hapus, `Some(Some(x))` = set.
#[derive(Debug, Default)]
pub struct UserPatch {
    pub role: Option<Role>,
    pub disabled: Option<bool>,
    pub github_login: Option<Option<String>>,
}

// `email` dan `github_login` bertipe citext; cast ke text supaya cocok dengan String.
pub(crate) const COLUMNS: &str = "id, email::text AS email, password_hash, role, disabled, \
                                  github_login::text AS github_login, created_at";

pub async fn count(pool: &PgPool) -> Result<i64> {
    sqlx::query_scalar("SELECT count(*) FROM users")
        .fetch_one(pool)
        .await
}

pub async fn count_active_admins(pool: &PgPool) -> Result<i64> {
    sqlx::query_scalar("SELECT count(*) FROM users WHERE role = 'admin' AND NOT disabled")
        .fetch_one(pool)
        .await
}

pub async fn insert(
    pool: &PgPool,
    email: &str,
    password_hash: &str,
    role: Role,
    github_login: Option<&str>,
) -> Result<User> {
    sqlx::query_as(&format!(
        "INSERT INTO users (email, password_hash, role, github_login) \
         VALUES ($1, $2, $3, $4::citext) RETURNING {COLUMNS}"
    ))
    .bind(email)
    .bind(password_hash)
    .bind(role)
    .bind(github_login)
    .fetch_one(pool)
    .await
}

/// Buat admin pertama hanya jika tabel `users` kosong; `None` = sudah ada user.
/// Tabel dikunci selama transaksi supaya dua setup bersamaan tidak membuat dua admin.
pub async fn insert_first_admin(
    pool: &PgPool,
    email: &str,
    password_hash: &str,
) -> Result<Option<User>> {
    let mut tx = pool.begin().await?;
    sqlx::query("LOCK TABLE users IN EXCLUSIVE MODE")
        .execute(&mut *tx)
        .await?;
    let existing: i64 = sqlx::query_scalar("SELECT count(*) FROM users")
        .fetch_one(&mut *tx)
        .await?;
    if existing > 0 {
        return Ok(None);
    }
    let user = sqlx::query_as(&format!(
        "INSERT INTO users (email, password_hash, role) VALUES ($1, $2, 'admin') RETURNING {COLUMNS}"
    ))
    .bind(email)
    .bind(password_hash)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Some(user))
}

pub async fn by_email(pool: &PgPool, email: &str) -> Result<Option<User>> {
    sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM users WHERE email = $1::citext"
    ))
    .bind(email)
    .fetch_optional(pool)
    .await
}

pub async fn by_id(pool: &PgPool, id: Uuid) -> Result<Option<User>> {
    sqlx::query_as(&format!("SELECT {COLUMNS} FROM users WHERE id = $1"))
        .bind(id)
        .fetch_optional(pool)
        .await
}

/// Case-insensitive (kolom citext).
pub async fn by_github_login(pool: &PgPool, login: &str) -> Result<Option<User>> {
    sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM users WHERE github_login = $1::citext"
    ))
    .bind(login)
    .fetch_optional(pool)
    .await
}

pub async fn list(pool: &PgPool) -> Result<Vec<User>> {
    sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM users ORDER BY created_at, id"
    ))
    .fetch_all(pool)
    .await
}

pub async fn update(pool: &PgPool, id: Uuid, patch: UserPatch) -> Result<User> {
    let mut conn = pool.acquire().await?;
    apply_patch(&mut conn, id, patch).await
}

/// Satu-satunya `UPDATE` untuk `UserPatch`; dipakai `update` dan `update_guarded`.
async fn apply_patch(conn: &mut PgConnection, id: Uuid, patch: UserPatch) -> Result<User> {
    // $4 menandai apakah github_login diubah (termasuk dikosongkan); $5 nilai barunya.
    sqlx::query_as(&format!(
        "UPDATE users SET            role = COALESCE($2, role),            disabled = COALESCE($3, disabled),            github_login = CASE WHEN $4 THEN $5::citext ELSE github_login END          WHERE id = $1 RETURNING {COLUMNS}"
    ))
    .bind(id)
    .bind(patch.role)
    .bind(patch.disabled)
    .bind(patch.github_login.is_some())
    .bind(patch.github_login.flatten())
    .fetch_one(conn)
    .await
}

/// Hasil `update_guarded`.
#[derive(Debug)]
pub enum GuardedUpdate {
    Updated(User),
    NotFound,
    /// Perubahan akan menghilangkan admin aktif terakhir; tidak ada yang diubah.
    LastAdmin,
}

/// Seperti `update`, tetapi menolak (`LastAdmin`) menurunkan peran atau menonaktifkan admin aktif terakhir.
/// Baris admin aktif dikunci (`FOR UPDATE`) selama transaksi, jadi dua perubahan bersamaan
/// diproses bergantian dan tidak bisa sama-sama lolos cek.
pub async fn update_guarded(pool: &PgPool, id: Uuid, patch: UserPatch) -> Result<GuardedUpdate> {
    let mut tx = pool.begin().await?;
    let admins: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM users WHERE role = 'admin' AND NOT disabled ORDER BY id FOR UPDATE",
    )
    .fetch_all(&mut *tx)
    .await?;
    // Baris target dikunci juga supaya cek di bawah tidak basi.
    let target: Option<(Role, bool)> =
        sqlx::query_as("SELECT role, disabled FROM users WHERE id = $1 FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
    let Some((role, disabled)) = target else {
        return Ok(GuardedUpdate::NotFound);
    };

    let is_active_admin = role == Role::Admin && !disabled;
    let loses_admin = patch.role == Some(Role::User) || patch.disabled == Some(true);
    if is_active_admin && loses_admin && admins.len() == 1 {
        return Ok(GuardedUpdate::LastAdmin);
    }

    let user = apply_patch(&mut tx, id, patch).await?;
    tx.commit().await?;
    Ok(GuardedUpdate::Updated(user))
}

pub async fn set_password(pool: &PgPool, id: Uuid, password_hash: &str) -> Result<()> {
    sqlx::query("UPDATE users SET password_hash = $2 WHERE id = $1")
        .bind(id)
        .bind(password_hash)
        .execute(pool)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[sqlx::test(migrations = "../../migrations")]
    async fn email_is_unique_case_insensitive(pool: PgPool) {
        insert(&pool, "Ann@Example.com", "h", Role::User, None)
            .await
            .unwrap();
        assert!(
            insert(&pool, "ann@example.COM", "h", Role::User, None)
                .await
                .is_err()
        );
        let found = by_email(&pool, "ANN@example.com").await.unwrap().unwrap();
        assert_eq!(found.email, "Ann@Example.com");
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn first_admin_only_when_table_empty(pool: PgPool) {
        let first = insert_first_admin(&pool, "a@x.io", "h").await.unwrap();
        assert_eq!(first.unwrap().role, Role::Admin);
        assert!(
            insert_first_admin(&pool, "b@x.io", "h")
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(count(&pool).await.unwrap(), 1);
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn count_active_admins_ignores_disabled(pool: PgPool) {
        let a = insert(&pool, "a@x.io", "h", Role::Admin, None)
            .await
            .unwrap();
        insert(&pool, "b@x.io", "h", Role::Admin, None)
            .await
            .unwrap();
        insert(&pool, "c@x.io", "h", Role::User, None)
            .await
            .unwrap();
        assert_eq!(count_active_admins(&pool).await.unwrap(), 2);

        let patch = UserPatch {
            disabled: Some(true),
            ..Default::default()
        };
        update(&pool, a.id, patch).await.unwrap();
        assert_eq!(count_active_admins(&pool).await.unwrap(), 1);
        assert_eq!(count(&pool).await.unwrap(), 3);
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn github_login_set_clear_and_lookup(pool: PgPool) {
        let u = insert(&pool, "a@x.io", "h", Role::User, None)
            .await
            .unwrap();
        let set = UserPatch {
            github_login: Some(Some("Octo".into())),
            ..Default::default()
        };
        let u = update(&pool, u.id, set).await.unwrap();
        assert_eq!(u.github_login.as_deref(), Some("Octo"));
        assert_eq!(
            by_github_login(&pool, "octo").await.unwrap().unwrap().id,
            u.id
        );

        // None = biarkan
        let u = update(&pool, u.id, UserPatch::default()).await.unwrap();
        assert_eq!(u.github_login.as_deref(), Some("Octo"));

        let clear = UserPatch {
            github_login: Some(None),
            ..Default::default()
        };
        let u = update(&pool, u.id, clear).await.unwrap();
        assert!(u.github_login.is_none());
        assert!(by_github_login(&pool, "octo").await.unwrap().is_none());
    }

    fn demote() -> UserPatch {
        UserPatch {
            role: Some(Role::User),
            ..Default::default()
        }
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn guarded_update_refuses_last_active_admin(pool: PgPool) {
        let admin = insert(&pool, "a@x.io", "h", Role::Admin, None)
            .await
            .unwrap();
        let disable = UserPatch {
            disabled: Some(true),
            ..Default::default()
        };
        assert!(matches!(
            update_guarded(&pool, admin.id, demote()).await.unwrap(),
            GuardedUpdate::LastAdmin
        ));
        assert!(matches!(
            update_guarded(&pool, admin.id, disable).await.unwrap(),
            GuardedUpdate::LastAdmin
        ));
        assert_eq!(count_active_admins(&pool).await.unwrap(), 1);

        // Perubahan lain tetap boleh.
        let login = UserPatch {
            github_login: Some(Some("octo".into())),
            ..Default::default()
        };
        assert!(matches!(
            update_guarded(&pool, admin.id, login).await.unwrap(),
            GuardedUpdate::Updated(_)
        ));
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn guarded_update_allows_demoting_when_another_admin_exists(pool: PgPool) {
        let a = insert(&pool, "a@x.io", "h", Role::Admin, None)
            .await
            .unwrap();
        insert(&pool, "b@x.io", "h", Role::Admin, None)
            .await
            .unwrap();
        let GuardedUpdate::Updated(user) = update_guarded(&pool, a.id, demote()).await.unwrap()
        else {
            panic!("expected Updated");
        };
        assert_eq!(user.role, Role::User);
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn guarded_update_unknown_id_is_not_found(pool: PgPool) {
        let result = update_guarded(&pool, Uuid::new_v4(), demote())
            .await
            .unwrap();
        assert!(matches!(result, GuardedUpdate::NotFound));
    }

    /// Satu transaksi menahan kunci admin sambil menurunkan admin A; demosi B yang masuk
    /// bersamaan harus menunggu, lalu melihat A sudah bukan admin dan ditolak.
    #[sqlx::test(migrations = "../../migrations")]
    async fn concurrent_demotion_waits_for_admin_lock(pool: PgPool) {
        let a = insert(&pool, "a@x.io", "h", Role::Admin, None)
            .await
            .unwrap();
        let b = insert(&pool, "b@x.io", "h", Role::Admin, None)
            .await
            .unwrap();

        let mut tx = pool.begin().await.unwrap();
        sqlx::query("SELECT id FROM users WHERE role = 'admin' FOR UPDATE")
            .execute(&mut *tx)
            .await
            .unwrap();
        let other_pool = pool.clone();
        let other = tokio::spawn(async move { update_guarded(&other_pool, b.id, demote()).await });
        // Beri waktu agar `other` sampai ke kunci; tanpa kunci ia akan lolos dan test gagal.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        sqlx::query("UPDATE users SET role = 'user' WHERE id = $1")
            .bind(a.id)
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();

        let result = other.await.unwrap().unwrap();
        assert!(matches!(result, GuardedUpdate::LastAdmin));
        assert_eq!(count_active_admins(&pool).await.unwrap(), 1);
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn insert_with_github_login(pool: PgPool) {
        let u = insert(&pool, "a@x.io", "h", Role::User, Some("Octo"))
            .await
            .unwrap();
        assert_eq!(u.github_login.as_deref(), Some("Octo"));
    }
}
