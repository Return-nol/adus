use adus_core::types::ReviewMode;
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

use crate::Result;

/// Pengaturan global; tabelnya selalu berisi tepat satu baris (dibuat oleh migrasi).
#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub struct Settings {
    pub default_provider_id: Option<Uuid>,
    pub default_model: Option<String>,
    pub max_files_per_review: i32,
    pub max_patch_lines_per_file: i32,
    /// Pola glob file yang dilewati reviewer; NULL = pakai bawaan (lihat `ignore::effective_ignore_paths`).
    pub ignore_paths: Option<Vec<String>>,
    /// Judul PR yang memuat salah satu kata ini membuat review otomatis dilewati.
    pub ignored_title_keywords: Vec<String>,
    /// Global default review mode; a repository can override it.
    pub review_mode: ReviewMode,
}

const COLUMNS: &str = "default_provider_id, default_model, max_files_per_review, \
     max_patch_lines_per_file, ignore_paths, ignored_title_keywords, review_mode";

pub async fn get(pool: &PgPool) -> Result<Settings> {
    sqlx::query_as(&format!("SELECT {COLUMNS} FROM settings WHERE id = 1"))
        .fetch_one(pool)
        .await
}

/// Mengganti seluruh nilai; pemanggil membaca dulu dengan `get` untuk perubahan parsial.
pub async fn update(pool: &PgPool, new: &Settings) -> Result<Settings> {
    sqlx::query_as(&format!(
        "UPDATE settings SET default_provider_id = $1, default_model = $2, \
         max_files_per_review = $3, max_patch_lines_per_file = $4, \
         ignore_paths = $5, ignored_title_keywords = $6, review_mode = $7 WHERE id = 1 \
         RETURNING {COLUMNS}"
    ))
    .bind(new.default_provider_id)
    .bind(&new.default_model)
    .bind(new.max_files_per_review)
    .bind(new.max_patch_lines_per_file)
    .bind(&new.ignore_paths)
    .bind(&new.ignored_title_keywords)
    .bind(new.review_mode)
    .fetch_one(pool)
    .await
}
