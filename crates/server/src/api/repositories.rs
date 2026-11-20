use adus_db::{
    connections,
    repositories::{self, NewRepository, RepoPatch, Repository},
};
use adus_git::RepoRef;
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    routing::{get, patch},
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize};
use uuid::Uuid;

use super::connections::{describe, git_error, mark_invalid_if_unauthorized, provider_for};
use crate::{
    auth::{AdminUser, CurrentUser, random_token},
    error::{ApiError, ApiJson, ApiPath, ApiQuery},
    state::AppState,
};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/repositories", get(list).post(create))
        .route("/repositories/{id}", patch(update).delete(remove))
}

#[derive(Deserialize)]
struct ListQuery {
    connection_id: Option<Uuid>,
}

#[derive(Deserialize)]
struct CreateRepository {
    connection_id: Uuid,
    external_id: String,
}

#[derive(Deserialize)]
struct UpdateRepository {
    enabled: Option<bool>,
    auto_review: Option<bool>,
    /// Ada dan `null` = kosongkan; tidak ada = biarkan.
    #[serde(default, deserialize_with = "present_or_null")]
    review_provider_id: Option<Option<Uuid>>,
    #[serde(default, deserialize_with = "present_or_null")]
    review_model: Option<Option<String>>,
}

/// Membedakan field yang tidak dikirim (`None`) dari field yang dikirim `null` (`Some(None)`).
pub(super) fn present_or_null<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

/// Respons repo. Token webhook sengaja tidak disertakan.
#[derive(Serialize)]
struct RepositoryJson {
    id: Uuid,
    connection_id: Uuid,
    external_id: String,
    owner: String,
    project: Option<String>,
    project_name: Option<String>,
    /// Azure `project/repo`, GitHub `owner/repo`.
    display_name: String,
    name: String,
    default_branch: Option<String>,
    enabled: bool,
    auto_review: bool,
    review_provider_id: Option<Uuid>,
    review_model: Option<String>,
    webhook_count: usize,
    created_at: DateTime<Utc>,
}

impl From<Repository> for RepositoryJson {
    fn from(r: Repository) -> RepositoryJson {
        RepositoryJson {
            display_name: repositories::display_name(&r),
            id: r.id,
            connection_id: r.connection_id,
            external_id: r.external_id,
            owner: r.owner,
            project: r.project,
            project_name: r.project_name,
            name: r.name,
            default_branch: r.default_branch,
            enabled: r.enabled,
            auto_review: r.auto_review,
            review_provider_id: r.review_provider_id,
            review_model: r.review_model,
            webhook_count: r.webhook_external_ids.len(),
            created_at: r.created_at,
        }
    }
}

/// Rujukan repo untuk `GitProvider`. GitHub: `project` kosong. Azure: `project` = GUID project.
pub(crate) fn repo_ref(repo: &Repository) -> RepoRef {
    RepoRef {
        external_id: repo.external_id.clone(),
        owner: repo.owner.clone(),
        project: repo.project.clone(),
        name: repo.name.clone(),
    }
}

/// Secret yang dikirim ke provider (HMAC GitHub; Azure mengabaikannya dan memakai `?token=` di URL).
pub(super) fn webhook_secret(repo: &Repository) -> &str {
    repo.webhook_token.as_deref().unwrap_or_default()
}

async fn list(
    _user: CurrentUser,
    State(state): State<AppState>,
    ApiQuery(query): ApiQuery<ListQuery>,
) -> Result<Json<Vec<RepositoryJson>>, ApiError> {
    let repos = repositories::list(&state.db, query.connection_id).await?;
    Ok(Json(repos.into_iter().map(RepositoryJson::from).collect()))
}

async fn create(
    _admin: AdminUser,
    State(state): State<AppState>,
    ApiJson(body): ApiJson<CreateRepository>,
) -> Result<(StatusCode, Json<RepositoryJson>), ApiError> {
    let c = connections::get(&state.db, body.connection_id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    let provider = provider_for(&state, &c)?;

    let remote = match provider.list_repos().await {
        Ok(remote) => remote,
        Err(err) => {
            mark_invalid_if_unauthorized(&state, c.id, &err).await?;
            return Err(git_error(&err));
        }
    };
    let remote = remote
        .into_iter()
        .find(|r| r.r#ref.external_id == body.external_id)
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "remote_repo_not_found",
                "this connection cannot see that repository",
            )
        })?;

    // Baris dibuat dulu karena URL webhook memuat id repo.
    let token = random_token();
    let r = &remote.r#ref;
    let inserted = repositories::insert(
        &state.db,
        NewRepository {
            connection_id: c.id,
            external_id: &r.external_id,
            owner: &r.owner,
            project: r.project.as_deref(),
            project_name: remote.project_name.as_deref(),
            name: &r.name,
            default_branch: remote.default_branch.as_deref(),
            webhook_token: &token,
        },
    )
    .await?;
    let Some(mut repo) = inserted else {
        return Err(ApiError::conflict(
            "repository_exists",
            "this repository is already added",
        ));
    };

    let url = repositories::webhook_url(&state.config.public_url, &repo, c.kind);
    match provider
        .ensure_webhooks(&repo_ref(&repo), &url, &token, &[])
        .await
    {
        Ok(ids) => {
            repositories::set_webhook_ids(&state.db, repo.id, &ids).await?;
            repo.webhook_external_ids = ids;
            Ok((StatusCode::CREATED, Json(repo.into())))
        }
        Err(err) => {
            // Tanpa webhook repo ini tidak berguna: batalkan baris yang baru dibuat.
            repositories::discard(&state.db, repo.id).await?;
            mark_invalid_if_unauthorized(&state, c.id, &err).await?;
            tracing::warn!("webhook registration failed for {}: {err}", r.name);
            Err(ApiError::new(
                StatusCode::BAD_GATEWAY,
                "webhook_failed",
                format!(
                    "could not register the webhook: {}{}",
                    describe(&err),
                    public_url_hint(&state.config.public_url)
                ),
            ))
        }
    }
}

/// Tambahan pesan gagal-webhook jika `ADUS_PUBLIC_URL` jelas tidak bisa dijangkau provider
/// (bukan https, atau host lokal).
fn public_url_hint(public_url: &str) -> &'static str {
    let public = reqwest::Url::parse(public_url).is_ok_and(|url| {
        url.scheme() == "https"
            && !matches!(
                url.host_str(),
                None | Some("localhost" | "127.0.0.1" | "0.0.0.0")
            )
    });
    if public {
        ""
    } else {
        " — GitHub/Azure must be able to reach ADUS_PUBLIC_URL (currently not public)"
    }
}

async fn update(
    _admin: AdminUser,
    State(state): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
    ApiJson(body): ApiJson<UpdateRepository>,
) -> Result<Json<RepositoryJson>, ApiError> {
    let patch = RepoPatch {
        enabled: body.enabled,
        auto_review: body.auto_review,
        review_provider_id: body.review_provider_id,
        // String kosong sama dengan menghapus override.
        review_model: body
            .review_model
            .map(|m| m.map(|m| m.trim().to_string()).filter(|m| !m.is_empty())),
    };
    let repo = repositories::update(&state.db, id, patch)
        .await?
        .ok_or_else(ApiError::not_found)?;
    Ok(Json(repo.into()))
}

async fn remove(
    _admin: AdminUser,
    State(state): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
) -> Result<StatusCode, ApiError> {
    let repo = repositories::get(&state.db, id)
        .await?
        .ok_or_else(ApiError::not_found)?;

    // Best-effort: gagal di provider (token mati, repo hilang) tidak boleh menahan penghapusan.
    if let Some(c) = connections::get(&state.db, repo.connection_id).await?
        && let Ok(provider) = provider_for(&state, &c)
        && let Err(err) = provider
            .delete_webhooks(&repo_ref(&repo), &repo.webhook_external_ids)
            .await
    {
        tracing::warn!("could not delete webhooks of {}: {err}", repo.name);
    }

    repositories::soft_delete(&state.db, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::public_url_hint;

    #[test]
    fn hint_only_for_non_public_urls() {
        for url in [
            "http://adus.example.com",
            "https://localhost:8080",
            "https://127.0.0.1",
            "https://0.0.0.0:8080",
            "not a url",
        ] {
            assert!(
                public_url_hint(url).contains("currently not public"),
                "{url}"
            );
        }
        assert_eq!(public_url_hint("https://adus.example.com"), "");
    }
}
