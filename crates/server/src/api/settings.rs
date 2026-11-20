use std::ops::RangeInclusive;

use adus_db::{
    ai,
    settings::{self, Settings},
};
use axum::{Json, Router, extract::State, routing::get};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    auth::{AdminUser, CurrentUser},
    error::{ApiError, ApiJson},
    state::AppState,
};

pub fn routes() -> Router<AppState> {
    Router::new().route("/settings", get(read).put(replace))
}

const FILES_RANGE: RangeInclusive<i32> = 1..=500;
const PATCH_LINES_RANGE: RangeInclusive<i32> = 1..=100_000;

/// Dipakai untuk GET dan PUT: PUT mengganti seluruh nilai.
#[derive(Serialize, Deserialize)]
struct SettingsJson {
    default_provider_id: Option<Uuid>,
    default_model: Option<String>,
    max_files_per_review: i32,
    max_patch_lines_per_file: i32,
}

impl From<Settings> for SettingsJson {
    fn from(s: Settings) -> SettingsJson {
        SettingsJson {
            default_provider_id: s.default_provider_id,
            default_model: s.default_model,
            max_files_per_review: s.max_files_per_review,
            max_patch_lines_per_file: s.max_patch_lines_per_file,
        }
    }
}

async fn read(
    _user: CurrentUser,
    State(state): State<AppState>,
) -> Result<Json<SettingsJson>, ApiError> {
    Ok(Json(settings::get(&state.db).await?.into()))
}

async fn replace(
    _admin: AdminUser,
    State(state): State<AppState>,
    ApiJson(body): ApiJson<SettingsJson>,
) -> Result<Json<SettingsJson>, ApiError> {
    if !FILES_RANGE.contains(&body.max_files_per_review) {
        return Err(ApiError::validation(
            "max_files_per_review must be between 1 and 500",
        ));
    }
    if !PATCH_LINES_RANGE.contains(&body.max_patch_lines_per_file) {
        return Err(ApiError::validation(
            "max_patch_lines_per_file must be between 1 and 100000",
        ));
    }
    let model = body
        .default_model
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .map(str::to_string);
    if let Some(provider_id) = body.default_provider_id {
        if model.is_none() {
            return Err(ApiError::validation(
                "default_model is required when default_provider_id is set",
            ));
        }
        if ai::get_provider(&state.db, provider_id).await?.is_none() {
            return Err(ApiError::not_found());
        }
    }
    // Daftar ignore belum bisa diubah lewat API ini; nilai yang ada dipertahankan.
    let current = settings::get(&state.db).await?;
    let new = Settings {
        default_provider_id: body.default_provider_id,
        default_model: model,
        max_files_per_review: body.max_files_per_review,
        max_patch_lines_per_file: body.max_patch_lines_per_file,
        ignore_paths: current.ignore_paths,
        ignored_title_keywords: current.ignored_title_keywords,
    };
    Ok(Json(settings::update(&state.db, &new).await?.into()))
}
