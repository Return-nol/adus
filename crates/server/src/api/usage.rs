use adus_db::usage::{self, BreakdownBy, DailyUsage, UsageBreakdown};
use axum::{Json, Router, extract::State, routing::get};
use chrono::{Duration, NaiveDate, Utc};
use serde::Deserialize;

use crate::{
    auth::CurrentUser,
    error::{ApiError, ApiQuery},
    state::AppState,
};

/// Rentang terpanjang yang boleh diminta (hari, inklusif).
const MAX_SPAN_DAYS: i64 = 366;
/// Rentang bawaan: 30 hari kalender UTC yang berakhir hari ini.
const DEFAULT_SPAN_DAYS: i64 = 30;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/usage/daily", get(daily))
        .route("/usage/breakdown", get(breakdown))
}

#[derive(Deserialize)]
struct Range {
    from: Option<NaiveDate>,
    to: Option<NaiveDate>,
}

#[derive(Deserialize)]
struct BreakdownQuery {
    from: Option<NaiveDate>,
    to: Option<NaiveDate>,
    by: BreakdownBy,
}

/// Isi default, lalu tolak rentang terbalik atau terlalu panjang (422).
fn resolve(
    from: Option<NaiveDate>,
    to: Option<NaiveDate>,
) -> Result<(NaiveDate, NaiveDate), ApiError> {
    let to = to.unwrap_or_else(|| Utc::now().date_naive());
    let from = from.unwrap_or(to - Duration::days(DEFAULT_SPAN_DAYS - 1));
    if to < from {
        return Err(ApiError::validation("to must not be before from"));
    }
    if (to - from).num_days() + 1 > MAX_SPAN_DAYS {
        return Err(ApiError::validation("range must not exceed 366 days"));
    }
    Ok((from, to))
}

async fn daily(
    _user: CurrentUser,
    State(state): State<AppState>,
    ApiQuery(q): ApiQuery<Range>,
) -> Result<Json<Vec<DailyUsage>>, ApiError> {
    let (from, to) = resolve(q.from, q.to)?;
    Ok(Json(usage::daily(&state.db, from, to).await?))
}

async fn breakdown(
    _user: CurrentUser,
    State(state): State<AppState>,
    ApiQuery(q): ApiQuery<BreakdownQuery>,
) -> Result<Json<Vec<UsageBreakdown>>, ApiError> {
    let (from, to) = resolve(q.from, q.to)?;
    Ok(Json(usage::breakdown(&state.db, q.by, from, to).await?))
}
