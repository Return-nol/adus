use std::{collections::HashSet, sync::Arc};

use adus_db::{
    connections::{self, Connection, ConnectionStatus, DeleteOutcome},
    repositories::{self, Repository},
};
use adus_git::{ConnectionKind, GitError, GitProvider, RemoteRepo};
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    routing::{get, patch, post},
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::repositories::{repo_ref, webhook_secret};
use crate::{
    auth::{AdminUser, CurrentUser},
    error::{ApiError, ApiJson, ApiPath},
    state::AppState,
};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/connections", get(list).post(create))
        .route("/connections/{id}", patch(update).delete(remove))
        .route("/connections/{id}/resync-webhooks", post(resync_webhooks))
        .route("/connections/{id}/remote-repos", get(remote_repos))
}

#[derive(Deserialize)]
struct CreateConnection {
    kind: ConnectionKind,
    name: String,
    org_or_owner: String,
    token: String,
}

#[derive(Deserialize)]
struct UpdateConnection {
    name: Option<String>,
    token: Option<String>,
}

/// Respons connection. Tidak pernah berisi token, hanya 4 karakter terakhir.
#[derive(Serialize)]
struct ConnectionJson {
    id: Uuid,
    kind: ConnectionKind,
    name: String,
    org_or_owner: String,
    token_last4: String,
    status: ConnectionStatus,
    pat_owner: Option<String>,
    last_verified_at: Option<DateTime<Utc>>,
    repository_count: i64,
}

#[derive(Serialize)]
pub(super) struct WebhookError {
    repository_id: Uuid,
    message: String,
}

#[derive(Serialize)]
struct UpdateReply {
    #[serde(flatten)]
    connection: ConnectionJson,
    webhook_errors: Vec<WebhookError>,
}

#[derive(Serialize)]
struct ResyncReply {
    synced: usize,
    webhook_errors: Vec<WebhookError>,
}

#[derive(Serialize)]
struct RemoteRepoJson {
    #[serde(flatten)]
    repo: RemoteRepo,
    /// Sudah ditambahkan (aktif) di Adus.
    added: bool,
}

async fn connection_json(state: &AppState, c: Connection) -> Result<ConnectionJson, ApiError> {
    let repository_count = repositories::active_count_for_connection(&state.db, c.id).await?;
    Ok(ConnectionJson {
        id: c.id,
        kind: c.kind,
        name: c.name,
        org_or_owner: c.org_or_owner,
        token_last4: c.token_last4,
        status: c.status,
        pat_owner: c.pat_owner,
        last_verified_at: c.last_verified_at,
        repository_count,
    })
}

/// Teks aman untuk klien: dibuat dari jenis error, bukan dari pesan mentah provider.
pub(crate) fn describe(err: &GitError) -> &'static str {
    match err {
        GitError::Unauthorized => "the provider rejected the token",
        GitError::NotFound => "not found at the provider",
        GitError::RateLimited => "the provider rate limit was reached, try again later",
        GitError::Http(_) | GitError::Decode(_) => "the provider request failed",
    }
}

/// 422 `token_invalid` untuk token ditolak, selain itu 502 `provider_error`.
pub(super) fn git_error(err: &GitError) -> ApiError {
    match err {
        GitError::Unauthorized => ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "token_invalid",
            describe(err),
        ),
        _ => {
            tracing::warn!("git provider error: {err}");
            ApiError::new(StatusCode::BAD_GATEWAY, "provider_error", describe(err))
        }
    }
}

/// Token yang ditolak provider menandai connection `invalid`. Repo dan token lama tidak disentuh.
pub(super) async fn mark_invalid_if_unauthorized(
    state: &AppState,
    connection_id: Uuid,
    err: &GitError,
) -> Result<(), ApiError> {
    if *err == GitError::Unauthorized {
        connections::set_status(&state.db, connection_id, ConnectionStatus::Invalid).await?;
    }
    Ok(())
}

/// Buka token tersimpan dan bangun provider untuk connection ini.
pub(super) fn provider_for(
    state: &AppState,
    c: &Connection,
) -> Result<Arc<dyn GitProvider>, ApiError> {
    let token = state.cipher.decrypt(&c.encrypted_token).map_err(|err| {
        tracing::error!("cannot decrypt token of connection {}: {err}", c.id);
        ApiError::internal()
    })?;
    Ok(state.git.create(c.kind, &c.org_or_owner, &token))
}

async fn find(state: &AppState, id: Uuid) -> Result<Connection, ApiError> {
    connections::get(&state.db, id)
        .await?
        .ok_or_else(ApiError::not_found)
}

/// Pastikan webhook ada untuk semua repo aktif connection. Error per repo dikumpulkan, tidak fatal.
/// Return (jumlah repo yang sukses, error per repo).
async fn sync_webhooks(
    state: &AppState,
    c: &Connection,
    provider: &dyn GitProvider,
) -> Result<(usize, Vec<WebhookError>), ApiError> {
    let repos: Vec<Repository> = repositories::list(&state.db, Some(c.id)).await?;
    let mut synced = 0;
    let mut errors = Vec::new();
    for repo in repos {
        let url = repositories::webhook_url(&state.config.public_url, &repo, c.kind);
        let result = provider
            .ensure_webhooks(
                &repo_ref(&repo),
                &url,
                webhook_secret(&repo),
                &repo.webhook_external_ids,
            )
            .await;
        match result {
            Ok(ids) => {
                repositories::set_webhook_ids(&state.db, repo.id, &ids).await?;
                synced += 1;
            }
            Err(err) => {
                mark_invalid_if_unauthorized(state, c.id, &err).await?;
                errors.push(WebhookError {
                    repository_id: repo.id,
                    message: describe(&err).to_string(),
                });
            }
        }
    }
    Ok((synced, errors))
}

async fn list(
    _user: CurrentUser,
    State(state): State<AppState>,
) -> Result<Json<Vec<ConnectionJson>>, ApiError> {
    let mut out = Vec::new();
    for c in connections::list(&state.db).await? {
        out.push(connection_json(&state, c).await?);
    }
    Ok(Json(out))
}

pub(super) fn required(value: &str, field: &str) -> Result<String, ApiError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(ApiError::validation(format!("{field} must not be empty")));
    }
    Ok(value.to_string())
}

async fn create(
    _admin: AdminUser,
    State(state): State<AppState>,
    ApiJson(body): ApiJson<CreateConnection>,
) -> Result<(StatusCode, Json<ConnectionJson>), ApiError> {
    let name = required(&body.name, "name")?;
    let org_or_owner = required(&body.org_or_owner, "org_or_owner")?;
    let token = required(&body.token, "token")?;

    // Verifikasi dulu; gagal berarti tidak ada yang disimpan.
    let owner = state
        .git
        .create(body.kind, &org_or_owner, &token)
        .verify()
        .await
        .map_err(|err| git_error(&err))?;

    let c = connections::insert(
        &state.db,
        body.kind,
        &name,
        &org_or_owner,
        &state.cipher.encrypt(&token),
        &adus_core::crypto::last4(&token),
        &owner.login,
    )
    .await?;
    Ok((StatusCode::CREATED, Json(connection_json(&state, c).await?)))
}

async fn update(
    _admin: AdminUser,
    State(state): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
    ApiJson(body): ApiJson<UpdateConnection>,
) -> Result<Json<UpdateReply>, ApiError> {
    let mut c = find(&state, id).await?;
    let name = body
        .name
        .as_deref()
        .map(|n| required(n, "name"))
        .transpose()?;
    let token = body
        .token
        .as_deref()
        .map(|t| required(t, "token"))
        .transpose()?;

    // Token baru diverifikasi sebelum apa pun diubah. Gagal: connection lama utuh (token, nama, status).
    let mut new_provider = None;
    if let Some(token) = &token {
        let provider = state.git.create(c.kind, &c.org_or_owner, token);
        let owner = provider.verify().await.map_err(|err| git_error(&err))?;
        c = connections::update_token(
            &state.db,
            id,
            &state.cipher.encrypt(token),
            &adus_core::crypto::last4(token),
            &owner.login,
            ConnectionStatus::Ok,
        )
        .await?
        .ok_or_else(ApiError::not_found)?;
        new_provider = Some(provider);
    }
    if let Some(name) = &name {
        c = connections::update_name(&state.db, id, name)
            .await?
            .ok_or_else(ApiError::not_found)?;
    }

    // Setelah rotasi, periksa webhook tiap repo aktif dan buat ulang yang hilang.
    let mut webhook_errors = Vec::new();
    if let Some(provider) = new_provider {
        webhook_errors = sync_webhooks(&state, &c, provider.as_ref()).await?.1;
        c = find(&state, id).await?;
    }
    Ok(Json(UpdateReply {
        connection: connection_json(&state, c).await?,
        webhook_errors,
    }))
}

async fn remove(
    _admin: AdminUser,
    State(state): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
) -> Result<StatusCode, ApiError> {
    match connections::delete(&state.db, id).await? {
        DeleteOutcome::Deleted => Ok(StatusCode::NO_CONTENT),
        DeleteOutcome::NotFound => Err(ApiError::not_found()),
        DeleteOutcome::InUse => Err(ApiError::conflict(
            "connection_in_use",
            "remove the repositories of this connection first",
        )),
    }
}

async fn resync_webhooks(
    _admin: AdminUser,
    State(state): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
) -> Result<Json<ResyncReply>, ApiError> {
    let c = find(&state, id).await?;
    let provider = provider_for(&state, &c)?;
    let (synced, webhook_errors) = sync_webhooks(&state, &c, provider.as_ref()).await?;
    Ok(Json(ResyncReply {
        synced,
        webhook_errors,
    }))
}

async fn remote_repos(
    _admin: AdminUser,
    State(state): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
) -> Result<Json<Vec<RemoteRepoJson>>, ApiError> {
    let c = find(&state, id).await?;
    let provider = provider_for(&state, &c)?;
    let remote = match provider.list_repos().await {
        Ok(remote) => remote,
        Err(err) => {
            mark_invalid_if_unauthorized(&state, id, &err).await?;
            return Err(git_error(&err));
        }
    };
    let added: HashSet<String> = repositories::list(&state.db, Some(id))
        .await?
        .into_iter()
        .map(|r| r.external_id)
        .collect();
    Ok(Json(
        remote
            .into_iter()
            .map(|repo| RemoteRepoJson {
                added: added.contains(&repo.r#ref.external_id),
                repo,
            })
            .collect(),
    ))
}
