//! Akses Postgres: koneksi, migrasi, dan query per tabel.

pub mod connections;
pub mod repositories;
pub mod sessions;
pub mod users;

use sqlx::PgPool;

pub type Result<T> = std::result::Result<T, sqlx::Error>;

/// Konek ke Postgres lalu jalankan migrasi di `migrations/`.
pub async fn connect(url: &str) -> Result<PgPool> {
    let pool = PgPool::connect(url).await?;
    sqlx::migrate!("../../migrations").run(&pool).await?;
    Ok(pool)
}
