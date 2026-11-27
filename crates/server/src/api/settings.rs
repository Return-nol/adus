use std::ops::RangeInclusive;

use adus_core::types::ReviewMode;
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
const SMALL_PR_RANGE: RangeInclusive<i32> = 0..=10_000;
const INPUT_TOKENS_RANGE: RangeInclusive<i32> = 0..=100_000_000;
const THREAD_REPLIES_RANGE: RangeInclusive<i32> = 0..=100;
const VALIDATE_THREADS_RANGE: RangeInclusive<i32> = 0..=100;
const LEARNED_PER_PR_RANGE: RangeInclusive<i32> = 1..=50;
const MAX_KEYWORD_LEN: usize = 100;
const MAX_KEYWORDS: usize = 500;
const MAX_TEST_PATH_LEN: usize = 1000;

/// Respons GET dan PUT.
#[derive(Serialize)]
struct SettingsJson {
    default_provider_id: Option<Uuid>,
    max_files_per_review: i32,
    max_patch_lines_per_file: i32,
    /// Daftar yang berlaku: tersimpan, atau bawaan bila belum pernah disimpan.
    ignore_paths: Vec<String>,
    ignore_paths_is_default: bool,
    ignored_title_keywords: Vec<String>,
    review_mode: ReviewMode,
    small_pr_max_lines: i32,
    max_review_input_tokens: i32,
    max_thread_replies: i32,
    max_validate_threads: i32,
    max_learned_per_pr: i32,
    curator_effort: String,
}

impl From<Settings> for SettingsJson {
    fn from(s: Settings) -> SettingsJson {
        SettingsJson {
            ignore_paths: ignore::effective_ignore_paths(&s),
            ignore_paths_is_default: s.ignore_paths.is_none(),
            default_provider_id: s.default_provider_id,
            max_files_per_review: s.max_files_per_review,
            max_patch_lines_per_file: s.max_patch_lines_per_file,
            ignored_title_keywords: s.ignored_title_keywords,
            review_mode: s.review_mode,
            small_pr_max_lines: s.small_pr_max_lines,
            max_review_input_tokens: s.max_review_input_tokens,
            max_thread_replies: s.max_thread_replies,
            max_validate_threads: s.max_validate_threads,
            max_learned_per_pr: s.max_learned_per_pr,
            curator_effort: s.curator_effort,
        }
    }
}

#[derive(Deserialize)]
struct SettingsBody {
    default_provider_id: Option<Uuid>,
    max_files_per_review: i32,
    max_patch_lines_per_file: i32,
    /// Tidak ada = biarkan; `null` = kembali ke bawaan; array = ganti.
    #[serde(default, deserialize_with = "present_or_null")]
    ignore_paths: Option<Option<Vec<String>>>,
    /// Tidak ada = biarkan.
    ignored_title_keywords: Option<Vec<String>>,
    /// Tidak ada = biarkan.
    review_mode: Option<ReviewMode>,
    /// Tidak ada = biarkan.
    small_pr_max_lines: Option<i32>,
    /// Tidak ada = biarkan.
    max_review_input_tokens: Option<i32>,
    /// Tidak ada = biarkan.
    max_thread_replies: Option<i32>,
    /// Tidak ada = biarkan.
    max_validate_threads: Option<i32>,
    /// Tidak ada = biarkan.
    max_learned_per_pr: Option<i32>,
    /// Tidak ada = biarkan.
    curator_effort: Option<String>,
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
    if let Some(lines) = body.small_pr_max_lines
        && !SMALL_PR_RANGE.contains(&lines)
    {
        return Err(ApiError::validation(
            "small_pr_max_lines must be between 0 and 10000",
        ));
    }
    if let Some(tokens) = body.max_review_input_tokens
        && !INPUT_TOKENS_RANGE.contains(&tokens)
    {
        return Err(ApiError::validation(
            "max_review_input_tokens must be between 0 and 100000000",
        ));
    }
    if let Some(replies) = body.max_thread_replies
        && !THREAD_REPLIES_RANGE.contains(&replies)
    {
        return Err(ApiError::validation(
            "max_thread_replies must be between 0 and 100",
        ));
    }
    if let Some(threads) = body.max_validate_threads
        && !VALIDATE_THREADS_RANGE.contains(&threads)
    {
        return Err(ApiError::validation(
            "max_validate_threads must be between 0 and 100",
        ));
    }
    if let Some(learned) = body.max_learned_per_pr
        && !LEARNED_PER_PR_RANGE.contains(&learned)
    {
        return Err(ApiError::validation(
            "max_learned_per_pr must be between 1 and 50",
        ));
    }
    if let Some(ref effort) = body.curator_effort
        && !matches!(effort.as_str(), "low" | "medium" | "high")
    {
        return Err(ApiError::validation(
            "curator_effort must be low, medium or high",
        ));
    }
    if let Some(provider_id) = body.default_provider_id
        && ai::get_provider(&state.db, provider_id).await?.is_none()
    {
        return Err(ApiError::not_found());
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
        max_files_per_review: body.max_files_per_review,
        max_patch_lines_per_file: body.max_patch_lines_per_file,
        ignore_paths,
        ignored_title_keywords,
        review_mode: body.review_mode.unwrap_or(current.review_mode),
        small_pr_max_lines: body
            .small_pr_max_lines
            .unwrap_or(current.small_pr_max_lines),
        max_review_input_tokens: body
            .max_review_input_tokens
            .unwrap_or(current.max_review_input_tokens),
        max_thread_replies: body
            .max_thread_replies
            .unwrap_or(current.max_thread_replies),
        max_validate_threads: body
            .max_validate_threads
            .unwrap_or(current.max_validate_threads),
        max_learned_per_pr: body
            .max_learned_per_pr
            .unwrap_or(current.max_learned_per_pr),
        curator_effort: body.curator_effort.unwrap_or(current.curator_effort),
        ..current
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
