use adus_core::types::ReviewMode;
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

use crate::Result;

/// Pengaturan global; tabelnya selalu berisi tepat satu baris (dibuat oleh migrasi).
#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub struct Settings {
    pub default_provider_id: Option<Uuid>,
    pub max_files_per_review: i32,
    pub max_patch_lines_per_file: i32,
    /// Pola glob file yang dilewati reviewer; NULL = pakai bawaan (lihat `ignore::effective_ignore_paths`).
    pub ignore_paths: Option<Vec<String>>,
    /// Judul PR yang memuat salah satu kata ini membuat review otomatis dilewati.
    pub ignored_title_keywords: Vec<String>,
    /// Global default review mode; a repository can override it.
    pub review_mode: ReviewMode,
    /// PRs with at most this many changed lines (and few, non-sensitive files) are reviewed
    /// diff-only. 0 = never.
    pub small_pr_max_lines: i32,
}

const COLUMNS: &str = "default_provider_id, max_files_per_review, \
     max_patch_lines_per_file, ignore_paths, ignored_title_keywords, review_mode, \n     small_pr_max_lines";

pub async fn get(pool: &PgPool) -> Result<Settings> {
    sqlx::query_as(&format!("SELECT {COLUMNS} FROM settings WHERE id = 1"))
        .fetch_one(pool)
        .await
}

/// Mengganti seluruh nilai; pemanggil membaca dulu dengan `get` untuk perubahan parsial.
pub async fn update(pool: &PgPool, new: &Settings) -> Result<Settings> {
    sqlx::query_as(&format!(
        "UPDATE settings SET default_provider_id = $1, \
         max_files_per_review = $2, max_patch_lines_per_file = $3, \
         ignore_paths = $4, ignored_title_keywords = $5, review_mode = $6, small_pr_max_lines = $7 WHERE id = 1 \
         RETURNING {COLUMNS}"
    ))
    .bind(new.default_provider_id)
    .bind(new.max_files_per_review)
    .bind(new.max_patch_lines_per_file)
    .bind(&new.ignore_paths)
    .bind(&new.ignored_title_keywords)
    .bind(new.review_mode)
    .bind(new.small_pr_max_lines)
    .fetch_one(pool)
    .await
}
