use adus_core::{job::Trigger, types::ConnectionKind};
use adus_db::reviews::{self, HistoryFilter, HistoryRow, ReviewStatus};
use axum::{Json, Router, extract::State, routing::get};
use chrono::{DateTime, Utc};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    auth::CurrentUser,
    error::{ApiError, ApiQuery},
    state::AppState,
};

const DEFAULT_PER_PAGE: i64 = 20;
const MAX_PER_PAGE: i64 = 100;

pub fn routes() -> Router<AppState> {
    Router::new().route("/reviews", get(list))
}

#[derive(Deserialize)]
struct ListQuery {
    repository_id: Option<Uuid>,
    status: Option<ReviewStatus>,
    page: Option<i64>,
    per_page: Option<i64>,
}

#[derive(Serialize)]
struct ReviewPage {
    items: Vec<ReviewJson>,
    total: i64,
}

#[derive(Serialize)]
struct RepoSummary {
    id: Uuid,
    /// `owner/name` (Azure: `org/name`).
    name: String,
    kind: ConnectionKind,
}

#[derive(Serialize)]
struct ReviewJson {
    id: Uuid,
    repository: RepoSummary,
    pr_number: i64,
    pr_url: Option<String>,
    trigger: Trigger,
    status: ReviewStatus,
    error: Option<String>,
    skip_reason: Option<String>,
    head_sha: Option<String>,
    comments_posted: i32,
    input_tokens: i64,
    output_tokens: i64,
    started_at: Option<DateTime<Utc>>,
    finished_at: Option<DateTime<Utc>>,
}

impl From<HistoryRow> for ReviewJson {
    fn from(r: HistoryRow) -> ReviewJson {
        ReviewJson {
            pr_url: pr_url(&r),
            repository: RepoSummary {
                id: r.repository_id,
                name: format!("{}/{}", r.repo_owner, r.repo_name),
                kind: r.connection_kind,
            },
            id: r.id,
            pr_number: r.pr_number,
            trigger: r.trigger,
            status: r.status,
            error: r.error,
            skip_reason: r.skip_reason,
            head_sha: r.head_sha,
            comments_posted: r.comments_posted,
            input_tokens: r.input_tokens,
            output_tokens: r.output_tokens,
            started_at: r.started_at,
            finished_at: r.finished_at,
        }
    }
}

/// Link web PR. Tiap segmen di-percent-encode (nama repo Azure boleh berisi spasi).
/// `None` untuk repo Azure tanpa project (seharusnya tidak terjadi).
fn pr_url(r: &HistoryRow) -> Option<String> {
    let number = r.pr_number.to_string();
    let (base, segments) = match r.connection_kind {
        ConnectionKind::Github => (
            "https://github.com",
            vec![r.repo_owner.as_str(), &r.repo_name, "pull", &number],
        ),
        ConnectionKind::AzureDevops => (
            "https://dev.azure.com",
            vec![
                r.repo_owner.as_str(),
                r.repo_project.as_deref()?,
                "_git",
                &r.repo_name,
                "pullrequest",
                &number,
            ],
        ),
    };
    let mut url = Url::parse(base).ok()?;
    url.path_segments_mut().ok()?.extend(segments);
    Some(url.into())
}

async fn list(
    _user: CurrentUser,
    State(state): State<AppState>,
    ApiQuery(q): ApiQuery<ListQuery>,
) -> Result<Json<ReviewPage>, ApiError> {
    let page = q.page.unwrap_or(1);
    let per_page = q.per_page.unwrap_or(DEFAULT_PER_PAGE);
    if page < 1 {
        return Err(ApiError::validation("page must be at least 1"));
    }
    if !(1..=MAX_PER_PAGE).contains(&per_page) {
        return Err(ApiError::validation("per_page must be between 1 and 100"));
    }
    let filter = HistoryFilter {
        repository_id: q.repository_id,
        status: q.status,
    };
    let (rows, total) = reviews::history(&state.db, &filter, page, per_page).await?;
    Ok(Json(ReviewPage {
        items: rows.into_iter().map(ReviewJson::from).collect(),
        total,
    }))
}
