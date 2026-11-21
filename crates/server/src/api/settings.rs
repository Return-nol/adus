use std::ops::RangeInclusive;

use adus_db::{
    ai,
    ignore::{self, IgnoreError, IgnoreMatcher},
    settings::{self, Settings},
};
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::repositories::present_or_null;
use crate::{
    auth::{AdminUser, CurrentUser},
    error::{ApiError, ApiJson},
    state::AppState,
};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/settings", get(read).put(replace))
        .route("/settings/ignore-test", post(ignore_test))
}

const FILES_RANGE: RangeInclusive<i32> = 1..=500;
const PATCH_LINES_RANGE: RangeInclusive<i32> = 1..=100_000;
const MAX_KEYWORD_LEN: usize = 100;
const MAX_KEYWORDS: usize = 500;
const MAX_TEST_PATH_LEN: usize = 1000;

/// Respons GET dan PUT.
#[derive(Serialize)]
struct SettingsJson {
    default_provider_id: Option<Uuid>,
    default_model: Option<String>,
    max_files_per_review: i32,
    max_patch_lines_per_file: i32,
    /// Daftar yang berlaku: tersimpan, atau bawaan bila belum pernah disimpan.
    ignore_paths: Vec<String>,
    ignore_paths_is_default: bool,
    ignored_title_keywords: Vec<String>,
}

impl From<Settings> for SettingsJson {
    fn from(s: Settings) -> SettingsJson {
        SettingsJson {
            ignore_paths: ignore::effective_ignore_paths(&s),
            ignore_paths_is_default: s.ignore_paths.is_none(),
            default_provider_id: s.default_provider_id,
            default_model: s.default_model,
            max_files_per_review: s.max_files_per_review,
            max_patch_lines_per_file: s.max_patch_lines_per_file,
            ignored_title_keywords: s.ignored_title_keywords,
        }
    }
}

#[derive(Deserialize)]
struct SettingsBody {
    default_provider_id: Option<Uuid>,
    default_model: Option<String>,
    max_files_per_review: i32,
    max_patch_lines_per_file: i32,
    /// Tidak ada = biarkan; `null` = kembali ke bawaan; array = ganti.
    #[serde(default, deserialize_with = "present_or_null")]
    ignore_paths: Option<Option<Vec<String>>>,
    /// Tidak ada = biarkan.
    ignored_title_keywords: Option<Vec<String>>,
}

impl From<IgnoreError> for ApiError {
    fn from(err: IgnoreError) -> ApiError {
        match err {
            IgnoreError::InvalidGlob(_) | IgnoreError::TooComplex => ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid_glob",
                err.to_string(),
            ),
            other => ApiError::validation(other.to_string()),
        }
    }
}

/// Trim, buang yang kosong dan duplikat (tanpa membedakan huruf besar/kecil, ejaan pertama dipakai).
fn normalize_keywords(raw: &[String]) -> Result<Vec<String>, ApiError> {
    let mut keywords: Vec<String> = Vec::new();
    for keyword in raw {
        let keyword = keyword.trim();
        if keyword.is_empty() {
            continue;
        }
        if keyword.chars().count() > MAX_KEYWORD_LEN {
            return Err(ApiError::validation(format!(
                "title keyword is longer than {MAX_KEYWORD_LEN} characters: {keyword}"
            )));
        }
        let lower = keyword.to_lowercase();
        if keywords.iter().any(|k| k.to_lowercase() == lower) {
            continue;
        }
        keywords.push(keyword.to_string());
    }
    if keywords.len() > MAX_KEYWORDS {
        return Err(ApiError::validation(format!(
            "more than {MAX_KEYWORDS} title keywords"
        )));
    }
    Ok(keywords)
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
    ApiJson(body): ApiJson<SettingsBody>,
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
    // Semua validasi selesai sebelum satu-satunya penulisan di bawah.
    let current = settings::get(&state.db).await?;
    let ignore_paths = match &body.ignore_paths {
        None => current.ignore_paths,
        Some(None) => None,
        Some(Some(raw)) => {
            let patterns = ignore::normalize_patterns(raw)?;
            // Kompilasi seluruh daftar sekarang, supaya engine tidak diam-diam kembali ke bawaan.
            IgnoreMatcher::new(&patterns)?;
            Some(patterns)
        }
    };
    let ignored_title_keywords = match &body.ignored_title_keywords {
        None => current.ignored_title_keywords,
        Some(raw) => normalize_keywords(raw)?,
    };
    let new = Settings {
        default_provider_id: body.default_provider_id,
        default_model: model,
        max_files_per_review: body.max_files_per_review,
        max_patch_lines_per_file: body.max_patch_lines_per_file,
        ignore_paths,
        ignored_title_keywords,
        review_mode: current.review_mode,
    };
    Ok(Json(settings::update(&state.db, &new).await?.into()))
}

#[derive(Deserialize)]
struct IgnoreTestBody {
    path: String,
}

#[derive(Serialize)]
struct IgnoreTestJson {
    ignored: bool,
    pattern: Option<String>,
}

/// Cek satu path terhadap daftar ignore yang berlaku.
async fn ignore_test(
    _user: CurrentUser,
    State(state): State<AppState>,
    ApiJson(body): ApiJson<IgnoreTestBody>,
) -> Result<Json<IgnoreTestJson>, ApiError> {
    let path = body.path.trim();
    if path.is_empty() || path.chars().count() > MAX_TEST_PATH_LEN {
        return Err(ApiError::validation(format!(
            "path must be 1 to {MAX_TEST_PATH_LEN} characters"
        )));
    }
    let patterns = ignore::effective_ignore_paths(&settings::get(&state.db).await?);
    let matcher = IgnoreMatcher::new(&patterns).map_err(|err| {
        tracing::error!("stored ignore list does not compile: {err}");
        ApiError::internal()
    })?;
    let pattern = matcher.matching_pattern(path).map(str::to_string);
    Ok(Json(IgnoreTestJson {
        ignored: pattern.is_some(),
        pattern,
    }))
}
