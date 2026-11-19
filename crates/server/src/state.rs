use std::sync::Arc;

use adus_core::{config::Config, crypto::Cipher};
use sqlx::PgPool;

/// State bersama untuk semua handler. Field lain (git, queue, chat) ditambahkan task berikutnya.
#[derive(Clone)]
pub struct AppState {
    pub db: PgPool,
    pub cipher: Arc<Cipher>,
    pub config: Arc<Config>,
}
