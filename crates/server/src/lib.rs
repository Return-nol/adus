pub mod api;
pub mod auth;
pub mod doctor;
pub mod error;
pub mod spa;
pub mod state;
pub mod webhooks;

use axum::{Router, routing::get};

pub use state::AppState;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .nest("/api/v1", auth::routes().merge(api::routes()))
        .merge(webhooks::routes())
        .fallback(spa::spa_handler)
        .with_state(state)
}

async fn healthz() -> &'static str {
    "ok"
}
