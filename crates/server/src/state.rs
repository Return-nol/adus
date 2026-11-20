use std::sync::Arc;

use adus_core::{config::Config, crypto::Cipher};
use adus_git::GitProviderFactory;
use adus_queue::Queue;
use sqlx::PgPool;

/// State bersama untuk semua handler. Field `chat` ditambahkan task berikutnya.
#[derive(Clone)]
pub struct AppState {
    pub db: PgPool,
    pub cipher: Arc<Cipher>,
    pub config: Arc<Config>,
    pub git: Arc<dyn GitProviderFactory>,
    /// Dipakai untuk tes key AI; tiap panggilan LLM memasang timeout-nya sendiri (120 dtk).
    pub http: reqwest::Client,
    /// Antrian job; webhook memasukkan job review/komentar ke sini.
    pub queue: Queue,
}
