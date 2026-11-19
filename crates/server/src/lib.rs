pub mod auth;
pub mod error;
pub mod state;

use axum::{Router, routing::get};

pub use state::AppState;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .nest("/api/v1", auth::routes())
        .with_state(state)
}

async fn healthz() -> &'static str {
    "ok"
}
