//! Endpoint webhook GitHub dan Azure DevOps. Tidak memakai sesi login: keaslian dibuktikan
//! dengan signature HMAC (GitHub) atau `?token=` (Azure), keduanya memakai `webhook_token` repo.
//!
//! Urutan pemeriksaan: repo tidak ada / terhapus / beda provider → 404; signature atau token
//! salah → 401; repo nonaktif → 200 `ignored`; event tak didukung → 200 `ignored`; delivery
//! sudah pernah dilihat → 200 `duplicate`; selain itu job di-enqueue → 202 `queued`.
//! Token, signature, dan isi payload tidak pernah dicatat di log.

use adus_core::{
    job::{CommentAuthor, Job, Trigger},
    types::{ConnectionKind, is_adus_comment},
};
use adus_db::{
    connections,
    repositories::{self, Repository},
};
use adus_queue::QueueError;
use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use hmac::{Hmac, Mac};
use http_body_util::LengthLimitError;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::Sha256;
use subtle::ConstantTimeEq;
use uuid::Uuid;

use crate::{
    error::{ApiError, ApiPath, ApiQuery},
    state::AppState,
};

/// Payload GitHub bisa sampai 25 MB. Body dibaca sendiri dengan batas ini (lihat `read_body`),
/// jadi batas default 2 MB axum tetap berlaku untuk route lain.
const MAX_BODY_BYTES: usize = 25 * 1024 * 1024;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/webhooks/github/{repo_id}", post(github))
        .route("/webhooks/azure/{repo_id}", post(azure))
}

#[derive(Deserialize)]
struct AzureQuery {
    token: Option<String>,
}

async fn github(
    State(state): State<AppState>,
    ApiPath(repo_id): ApiPath<Uuid>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, ApiError> {
    let repo = find_repo(&state, repo_id, ConnectionKind::Github).await?;
    let body = read_body(body).await?;
    let signature = header_str(&headers, "x-hub-signature-256");
    if !verify_github_signature(webhook_token(&repo), &body, signature) {
        return Err(invalid_signature());
    }
    if !repo.enabled {
        return Ok(status_reply(StatusCode::OK, "ignored"));
    }
    let payload = parse_json(&body)?;
    let event = header_str(&headers, "x-github-event").unwrap_or_default();
    let job = parse_github(event, &payload, repo.id);
    let dedup_key =
        header_str(&headers, "x-github-delivery").map(|id| format!("gh:{}:{id}", repo.id));
    queue_job(&state, job, dedup_key).await
}

async fn azure(
    State(state): State<AppState>,
    ApiPath(repo_id): ApiPath<Uuid>,
    ApiQuery(query): ApiQuery<AzureQuery>,
    body: Body,
) -> Result<Response, ApiError> {
    let repo = find_repo(&state, repo_id, ConnectionKind::AzureDevops).await?;
    // Token ada di URL, jadi diperiksa sebelum body di-buffer.
    let given = query.token.unwrap_or_default();
    if !tokens_match(webhook_token(&repo), &given) {
        return Err(invalid_signature());
    }
    let body = read_body(body).await?;
    if !repo.enabled {
        return Ok(status_reply(StatusCode::OK, "ignored"));
    }
    let payload = parse_json(&body)?;
    let job = parse_azure(&payload, repo.id);
    let dedup_key = payload["id"]
        .as_str()
        .map(|id| format!("az:{}:{id}", repo.id));
    queue_job(&state, job, dedup_key).await
}

/// Repo aktif yang connection-nya sesuai endpoint. Selain itu 404.
async fn find_repo(
    state: &AppState,
    repo_id: Uuid,
    kind: ConnectionKind,
) -> Result<Repository, ApiError> {
    let repo = repositories::get(&state.db, repo_id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    let connection = connections::get(&state.db, repo.connection_id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    if connection.kind != kind {
        return Err(ApiError::not_found());
    }
    Ok(repo)
}

/// Dibaca setelah repo ditemukan (dan untuk Azure setelah token cocok), supaya request yang
/// pasti ditolak tidak di-buffer.
async fn read_body(body: Body) -> Result<Bytes, ApiError> {
    axum::body::to_bytes(body, MAX_BODY_BYTES)
        .await
        .map_err(|err| {
            let too_large =
                std::error::Error::source(&err).is_some_and(|inner| inner.is::<LengthLimitError>());
            if too_large {
                ApiError::new(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "payload_too_large",
                    "webhook body is too large",
                )
            } else {
                ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "bad_request",
                    "failed to read request body",
                )
            }
        })
}

/// Token kosong (repo lama tanpa token) membuat semua verifikasi gagal.
fn webhook_token(repo: &Repository) -> &str {
    repo.webhook_token.as_deref().unwrap_or_default()
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

fn parse_json(body: &[u8]) -> Result<Value, ApiError> {
    serde_json::from_slice(body)
        .map_err(|_| ApiError::new(StatusCode::BAD_REQUEST, "bad_request", "invalid JSON body"))
}

fn invalid_signature() -> ApiError {
    ApiError::new(
        StatusCode::UNAUTHORIZED,
        "invalid_signature",
        "invalid webhook signature",
    )
}

fn status_reply(status: StatusCode, text: &str) -> Response {
    (status, Json(json!({ "status": text }))).into_response()
}

/// `None` = event diabaikan. Dengan `dedup_key`, penanda dan job ditulis atomik
/// (`Queue::enqueue_once`); tanpa key job tetap di-enqueue.
async fn queue_job(
    state: &AppState,
    job: Option<Job>,
    dedup_key: Option<String>,
) -> Result<Response, ApiError> {
    let Some(job) = job else {
        return Ok(status_reply(StatusCode::OK, "ignored"));
    };
    let queued = match dedup_key {
        Some(key) => state.queue.enqueue_once(&job, &key).await,
        None => state.queue.enqueue(&job).await.map(|()| true),
    }
    .map_err(queue_error)?;
    if !queued {
        return Ok(status_reply(StatusCode::OK, "duplicate"));
    }
    Ok(status_reply(StatusCode::ACCEPTED, "queued"))
}

fn queue_error(err: QueueError) -> ApiError {
    tracing::error!("webhook enqueue failed: {err}");
    ApiError::internal()
}

/// Cek header `X-Hub-Signature-256` (`sha256=<hex>`): HMAC-SHA256 body dengan `secret`,
/// dibandingkan constant-time. Secret kosong selalu ditolak.
fn verify_github_signature(secret: &str, body: &[u8], header: Option<&str>) -> bool {
    if secret.is_empty() {
        return false;
    }
    let Some(given) = header
        .and_then(|value| value.strip_prefix("sha256="))
        .and_then(|hex_text| hex::decode(hex_text).ok())
    else {
        return false;
    };
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes())
        .expect("HMAC-SHA256 accepts keys of any length");
    mac.update(body);
    mac.verify_slice(&given).is_ok()
}

/// Perbandingan constant-time untuk `?token=` Azure. Token tersimpan yang kosong selalu ditolak.
fn tokens_match(expected: &str, given: &str) -> bool {
    !expected.is_empty() && bool::from(expected.as_bytes().ct_eq(given.as_bytes()))
}

/// Ubah event GitHub (header `X-GitHub-Event`) menjadi job. `None` = abaikan.
fn parse_github(event: &str, body: &Value, repo_id: Uuid) -> Option<Job> {
    let action = body["action"].as_str()?;
    match event {
        "pull_request" => {
            let pr_number = pr_number(&body["pull_request"]["number"])?;
            let trigger = match action {
                "opened" | "ready_for_review" => Trigger::Opened,
                // Reopen: engine melewatinya jika head-nya sudah pernah direview.
                "reopened" | "synchronize" => Trigger::Updated,
                // Judul diubah: kata kunci judul yang diabaikan mungkin baru dihapus. Edit lain
                // (mis. deskripsi saja) diabaikan; head yang sama tetap dilewati engine.
                "edited" if !body["changes"]["title"].is_null() => Trigger::Updated,
                // Merged or not: learn from the PR's discussion.
                "closed" => {
                    return Some(Job::LearnPr {
                        repository_id: repo_id,
                        pr_number,
                    });
                }
                _ => return None,
            };
            Some(Job::ReviewPr {
                repository_id: repo_id,
                pr_number,
                trigger,
                mode_override: None,
            })
        }
        // `issue.pull_request` hanya ada jika issue itu sebuah PR.
        "issue_comment" if action == "created" && !body["issue"]["pull_request"].is_null() => {
            github_comment(body, &body["issue"]["number"], repo_id)
        }
        "pull_request_review_comment" if action == "created" => {
            github_comment(body, &body["pull_request"]["number"], repo_id)
        }
        _ => None,
    }
}

fn github_comment(body: &Value, number: &Value, repo_id: Uuid) -> Option<Job> {
    if body["sender"]["type"] == "Bot" {
        return None;
    }
    let comment = &body["comment"];
    let text = comment["body"].as_str()?;
    if is_adus_comment(text) {
        return None;
    }
    Some(Job::PrComment {
        repository_id: repo_id,
        pr_number: pr_number(number)?,
        comment_url: http_url(&comment["html_url"]).map(str::to_string),
        author: CommentAuthor {
            login: Some(comment["user"]["login"].as_str()?.to_string()),
            email: None,
        },
        body: text.to_string(),
    })
}

/// Ubah notifikasi service hook Azure DevOps menjadi job. `None` = abaikan.
fn parse_azure(body: &Value, repo_id: Uuid) -> Option<Job> {
    let resource = &body["resource"];
    let trigger = match body["eventType"].as_str()? {
        "git.pullrequest.created" => Trigger::Opened,
        // Update apa pun (push, judul, reviewer) → `Updated`; engine melewati head yang sama.
        "git.pullrequest.updated" => Trigger::Updated,
        "ms.vss-code.git-pullrequest-comment-event" => return azure_comment(resource, repo_id),
        _ => return None,
    };
    let pr_number = pr_number(&resource["pullRequestId"])?;
    // A closed PR (merged or abandoned) is learned from, not reviewed.
    if matches!(resource["status"].as_str(), Some("completed" | "abandoned")) {
        return Some(Job::LearnPr {
            repository_id: repo_id,
            pr_number,
        });
    }
    Some(Job::ReviewPr {
        repository_id: repo_id,
        pr_number,
        trigger,
        mode_override: None,
    })
}

fn azure_comment(resource: &Value, repo_id: Uuid) -> Option<Job> {
    let comment = &resource["comment"];
    if comment["commentType"] == "system" || comment["isDeleted"] == true {
        return None;
    }
    let text = comment["content"].as_str()?;
    if is_adus_comment(text) {
        return None;
    }
    let pr = &resource["pullRequest"];
    let pr_number = pr_number(&pr["pullRequestId"])?;
    let unique_name = comment["author"]["uniqueName"].as_str()?.to_string();
    let comment_url = http_url(&pr["repository"]["webUrl"])
        .map(|web_url| format!("{}/pullrequest/{pr_number}", web_url.trim_end_matches('/')));
    Some(Job::PrComment {
        repository_id: repo_id,
        pr_number,
        comment_url,
        author: CommentAuthor {
            login: Some(unique_name.clone()),
            email: Some(unique_name),
        },
        body: text.to_string(),
    })
}

/// Nomor PR harus bilangan bulat positif.
fn pr_number(value: &Value) -> Option<i64> {
    value.as_i64().filter(|number| *number > 0)
}

/// Hanya URL http(s) yang dipakai sebagai tautan (bukan `javascript:` dsb.).
fn http_url(value: &Value) -> Option<&str> {
    value
        .as_str()
        .filter(|url| url.starts_with("https://") || url.starts_with("http://"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const GH_PR_OPENED: &str =
        include_str!("../../../tests/fixtures/webhooks/github/pull_request_opened.json");
    const GH_ISSUE_COMMENT: &str =
        include_str!("../../../tests/fixtures/webhooks/github/issue_comment_created.json");
    const GH_PLAIN_ISSUE_COMMENT: &str =
        include_str!("../../../tests/fixtures/webhooks/github/issue_comment_plain_issue.json");
    const GH_REVIEW_COMMENT: &str = include_str!(
        "../../../tests/fixtures/webhooks/github/pull_request_review_comment_created.json"
    );
    const GH_PING: &str = include_str!("../../../tests/fixtures/webhooks/github/ping.json");
    const AZ_PR_CREATED: &str =
        include_str!("../../../tests/fixtures/webhooks/azure/pull_request_created.json");
    const AZ_PR_UPDATED: &str =
        include_str!("../../../tests/fixtures/webhooks/azure/pull_request_updated.json");
    const AZ_PR_COMMENT: &str =
        include_str!("../../../tests/fixtures/webhooks/azure/pull_request_comment.json");

    fn fixture(text: &str) -> Value {
        serde_json::from_str(text).unwrap()
    }

    fn learn(pr_number: i64) -> Option<Job> {
        Some(Job::LearnPr {
            repository_id: Uuid::nil(),
            pr_number,
        })
    }

    fn review(pr_number: i64, trigger: Trigger) -> Option<Job> {
        Some(Job::ReviewPr {
            repository_id: Uuid::nil(),
            pr_number,
            trigger,
            mode_override: None,
        })
    }

    fn sign(secret: &str, body: &[u8]) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(body);
        format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
    }

    #[test]
    fn signature_accepts_only_exact_hmac() {
        let body = br#"{"action":"opened"}"#;
        let good = sign("secret", body);
        assert!(verify_github_signature("secret", body, Some(&good)));
        assert!(verify_github_signature(
            "secret",
            body,
            Some(&good.to_uppercase().replace("SHA256=", "sha256="))
        ));

        assert!(!verify_github_signature("other", body, Some(&good)));
        assert!(!verify_github_signature("secret", b"{}", Some(&good)));
        assert!(!verify_github_signature("secret", body, None));
        assert!(!verify_github_signature("secret", body, Some(&good[7..])));
        assert!(!verify_github_signature(
            "secret",
            body,
            Some(&good[..good.len() - 2])
        ));
        assert!(!verify_github_signature("secret", body, Some("sha256=xyz")));
        assert!(!verify_github_signature("secret", body, Some("sha256=")));
        // Secret kosong tidak pernah lolos, walau signature dihitung dengan kunci kosong.
        assert!(!verify_github_signature("", body, Some(&sign("", body))));
    }

    #[test]
    fn azure_token_compare() {
        assert!(tokens_match("abc", "abc"));
        assert!(!tokens_match("abc", "abd"));
        assert!(!tokens_match("abc", "ab"));
        assert!(!tokens_match("", ""));
    }

    #[test]
    fn github_pull_request_actions() {
        let mut body = fixture(GH_PR_OPENED);
        let id = Uuid::nil();
        assert_eq!(
            parse_github("pull_request", &body, id),
            review(2, Trigger::Opened)
        );
        for (action, expected) in [
            ("reopened", review(2, Trigger::Updated)),
            ("ready_for_review", review(2, Trigger::Opened)),
            ("synchronize", review(2, Trigger::Updated)),
            ("closed", learn(2)),
            ("edited", None),
            ("labeled", None),
        ] {
            body["action"] = json!(action);
            assert_eq!(
                parse_github("pull_request", &body, id),
                expected,
                "{action}"
            );
        }
        body["action"] = json!("edited");
        body["changes"] = json!({ "body": { "from": "old description" } });
        assert_eq!(parse_github("pull_request", &body, id), None);
        body["changes"] = json!({ "title": { "from": "WIP: old title" } });
        assert_eq!(
            parse_github("pull_request", &body, id),
            review(2, Trigger::Updated)
        );
        body["changes"] = Value::Null;
        body["action"] = json!("opened");
        body["pull_request"]["number"] = json!(-1);
        assert_eq!(parse_github("pull_request", &body, id), None);
        body["pull_request"]["number"] = json!("2");
        assert_eq!(parse_github("pull_request", &body, id), None);
    }

    #[test]
    fn github_unknown_events_ignored() {
        let id = Uuid::nil();
        assert_eq!(parse_github("ping", &fixture(GH_PING), id), None);
        assert_eq!(parse_github("push", &fixture(GH_PR_OPENED), id), None);
        assert_eq!(parse_github("", &fixture(GH_PR_OPENED), id), None);
        assert_eq!(parse_github("pull_request", &json!([1, 2]), id), None);
    }

    #[test]
    fn github_issue_comment_on_pr() {
        let job = parse_github("issue_comment", &fixture(GH_ISSUE_COMMENT), Uuid::nil());
        assert_eq!(
            job,
            Some(Job::PrComment {
                repository_id: Uuid::nil(),
                pr_number: 2,
                comment_url: Some(
                    "https://github.com/Codertocat/Hello-World/pull/2#issuecomment-492700400"
                        .into()
                ),
                author: CommentAuthor {
                    login: Some("Codertocat".into()),
                    email: None,
                },
                body: "@adudu review".into(),
            })
        );
    }

    #[test]
    fn github_issue_comment_filters() {
        let id = Uuid::nil();
        assert_eq!(
            parse_github("issue_comment", &fixture(GH_PLAIN_ISSUE_COMMENT), id),
            None
        );

        let mut body = fixture(GH_ISSUE_COMMENT);
        body["action"] = json!("edited");
        assert_eq!(parse_github("issue_comment", &body, id), None);

        let mut body = fixture(GH_ISSUE_COMMENT);
        body["sender"]["type"] = json!("Bot");
        assert_eq!(parse_github("issue_comment", &body, id), None);

        let mut body = fixture(GH_ISSUE_COMMENT);
        body["comment"]["body"] = json!("<!-- adudu -->\nsummary");
        assert_eq!(parse_github("issue_comment", &body, id), None);

        // A human quoting an Adus comment is still a human comment.
        let mut body = fixture(GH_ISSUE_COMMENT);
        body["comment"]["body"] = json!("> <!-- adudu -->\nthanks @adudu review");
        assert!(matches!(
            parse_github("issue_comment", &body, id),
            Some(Job::PrComment { .. })
        ));

        // URL non-http(s) dibuang, job tetap jalan.
        let mut body = fixture(GH_ISSUE_COMMENT);
        body["comment"]["html_url"] = json!("javascript:alert(1)");
        let Some(Job::PrComment { comment_url, .. }) = parse_github("issue_comment", &body, id)
        else {
            panic!("expected a comment job");
        };
        assert_eq!(comment_url, None);
    }

    #[test]
    fn github_review_comment() {
        let id = Uuid::nil();
        let body = fixture(GH_REVIEW_COMMENT);
        let Some(Job::PrComment {
            pr_number,
            comment_url,
            author,
            body: text,
            ..
        }) = parse_github("pull_request_review_comment", &body, id)
        else {
            panic!("expected a comment job");
        };
        assert_eq!(pr_number, 2);
        assert_eq!(
            comment_url.as_deref(),
            Some("https://github.com/Codertocat/Hello-World/pull/2#discussion_r284312630")
        );
        assert_eq!(author.login.as_deref(), Some("Codertocat"));
        assert_eq!(text, "@adudu remember: we keep the README short");

        let mut edited = body.clone();
        edited["action"] = json!("edited");
        assert_eq!(
            parse_github("pull_request_review_comment", &edited, id),
            None
        );
    }

    #[test]
    fn azure_pull_request_events() {
        let id = Uuid::nil();
        assert_eq!(
            parse_azure(&fixture(AZ_PR_CREATED), id),
            review(1, Trigger::Opened)
        );
        assert_eq!(
            parse_azure(&fixture(AZ_PR_UPDATED), id),
            review(1, Trigger::Updated)
        );

        for status in ["completed", "abandoned"] {
            let mut body = fixture(AZ_PR_UPDATED);
            body["resource"]["status"] = json!(status);
            assert_eq!(parse_azure(&body, id), learn(1), "{status}");
        }

        let mut body = fixture(AZ_PR_CREATED);
        body["eventType"] = json!("git.push");
        assert_eq!(parse_azure(&body, id), None);
        assert_eq!(parse_azure(&json!({}), id), None);
    }

    #[test]
    fn azure_comment_event() {
        let id = Uuid::nil();
        assert_eq!(
            parse_azure(&fixture(AZ_PR_COMMENT), id),
            Some(Job::PrComment {
                repository_id: id,
                pr_number: 1,
                comment_url: Some(
                    "https://dev.azure.com/fabrikam/DefaultCollection/_git/Fabrikam/pullrequest/1"
                        .into()
                ),
                author: CommentAuthor {
                    login: Some("fabrikamfiber4@hotmail.com".into()),
                    email: Some("fabrikamfiber4@hotmail.com".into()),
                },
                body: "@adudu review".into(),
            })
        );

        let mut body = fixture(AZ_PR_COMMENT);
        body["resource"]["pullRequest"]["repository"]
            .as_object_mut()
            .unwrap()
            .remove("webUrl");
        let Some(Job::PrComment { comment_url, .. }) = parse_azure(&body, id) else {
            panic!("expected a comment job");
        };
        assert_eq!(comment_url, None);
    }

    #[test]
    fn azure_comment_filters() {
        let id = Uuid::nil();
        let mut body = fixture(AZ_PR_COMMENT);
        body["resource"]["comment"]["commentType"] = json!("system");
        assert_eq!(parse_azure(&body, id), None);

        let mut body = fixture(AZ_PR_COMMENT);
        body["resource"]["comment"]["isDeleted"] = json!(true);
        assert_eq!(parse_azure(&body, id), None);

        let mut body = fixture(AZ_PR_COMMENT);
        body["resource"]["comment"]["content"] = json!("<!-- adudu -->\nReview summary");
        assert_eq!(parse_azure(&body, id), None);

        let mut body = fixture(AZ_PR_COMMENT);
        body["resource"]["pullRequest"]["repository"]["webUrl"] = json!("ftp://example.test");
        let Some(Job::PrComment { comment_url, .. }) = parse_azure(&body, id) else {
            panic!("expected a comment job");
        };
        assert_eq!(comment_url, None);
    }
}
