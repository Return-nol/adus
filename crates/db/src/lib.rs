//! Akses Postgres: koneksi, migrasi, dan query per tabel.

pub mod ai;
pub mod connections;
pub mod memories;
pub mod repositories;
pub mod reviews;
pub mod rules;
pub mod sessions;
pub mod settings;
pub mod usage;
pub mod users;

use sqlx::{PgPool, migrate::Migrator};

pub type Result<T> = std::result::Result<T, sqlx::Error>;

/// Migrasi di `migrations/`, ikut ter-embed di binary.
static MIGRATOR: Migrator = sqlx::migrate!("../../migrations");

/// Konek ke Postgres lalu jalankan migrasi di `migrations/`.
pub async fn connect(url: &str) -> Result<PgPool> {
    let pool = connect_without_migrating(url).await?;
    MIGRATOR.run(&pool).await?;
    Ok(pool)
}

/// Konek saja, tanpa mengubah skema. Dipakai `adus doctor`, yang hanya membaca.
pub async fn connect_without_migrating(url: &str) -> Result<PgPool> {
    PgPool::connect(url).await
}

/// Versi migrasi bawaan binary yang belum terpasang dengan sukses (hilang atau `success = false`).
/// Hanya membaca; tidak menjalankan migrasi.
pub async fn unapplied_migrations(pool: &PgPool) -> Result<Vec<i64>> {
    let applied: Vec<i64> =
        sqlx::query_scalar("SELECT version FROM _sqlx_migrations WHERE success")
            .fetch_all(pool)
            .await?;
    Ok(MIGRATOR
        .iter()
        .filter(|m| m.migration_type.is_up_migration() && !applied.contains(&m.version))
        .map(|m| m.version)
        .collect())
}
