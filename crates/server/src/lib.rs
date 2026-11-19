use adus_core::config::Config;
use axum::{Router, routing::get};

#[derive(Clone)]
pub struct AppState {
    pub config: Config,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .with_state(state)
}

async fn healthz() -> &'static str {
    "ok"
}
