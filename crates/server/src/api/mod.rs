pub mod ai;
pub mod connections;
pub mod repositories;
pub mod settings;
pub mod usage;
pub mod users;

use axum::Router;

use crate::state::AppState;

/// Semua route dashboard di bawah `/api/v1` (selain auth).
pub fn routes() -> Router<AppState> {
    Router::new()
        .merge(users::routes())
        .merge(ai::routes())
        .merge(settings::routes())
        .merge(usage::routes())
        .merge(connections::routes())
        .merge(repositories::routes())
}
