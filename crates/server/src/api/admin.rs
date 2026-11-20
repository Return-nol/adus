use axum::{Json, Router, extract::State, routing::get};

use crate::{
    auth::AdminUser,
    doctor::{self, Report},
    state::AppState,
};

pub fn routes() -> Router<AppState> {
    Router::new().route("/admin/doctor", get(run_doctor))
}

/// Pengecekan yang sama dengan `adus doctor`; hanya membaca.
async fn run_doctor(_admin: AdminUser, State(state): State<AppState>) -> Json<Report> {
    Json(doctor::run(&state).await)
}
