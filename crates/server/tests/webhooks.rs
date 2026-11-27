//! Test endpoint webhook terhadap Postgres dan Redis asli. Tiap test memakai antrian dengan
//! prefix unik `test-{uuid}` dan menghapus key-nya di akhir.

mod common;

use std::collections::HashMap;

use adus_core::{
    job::{CommentAuthor, Job, Trigger},
    types::ConnectionKind,
};
use adus_db::{
    connections,
    repositories::{self, NewRepository, RepoPatch},
};
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
};
use hmac::{Hmac, Mac};
use serde_json::{Value, json};
use sha2::Sha256;
use sqlx::PgPool;
use uuid::Uuid;

const TOKEN: &str = "webhook-secret-token";

const GH_PR_OPENED: &str =
    include_str!("../../../tests/fixtures/webhooks/github/pull_request_opened.json");
const GH_ISSUE_COMMENT: &str =
    include_str!("../../../tests/fixtures/webhooks/github/issue_comment_created.json");
const GH_PLAIN_ISSUE_COMMENT: &str =
    include_str!("../../../tests/fixtures/webhooks/github/issue_comment_plain_issue.json");
const GH_PING: &str = include_str!("../../../tests/fixtures/webhooks/github/ping.json");
const AZ_PR_CREATED: &str =
    include_str!("../../../tests/fixtures/webhooks/azure/pull_request_created.json");
const AZ_PR_UPDATED: &str =
    include_str!("../../../tests/fixtures/webhooks/azure/pull_request_updated.json");
const AZ_PR_COMMENT: &str =
    include_str!("../../../tests/fixtures/webhooks/azure/pull_request_comment.json");

/// Satu repo aktif dengan `webhook_token = TOKEN` plus router dengan antriannya sendiri.
struct Hook {
    app: Router,
    pool: PgPool,
    prefix: String,
    repo_id: Uuid,
}

async fn setup(pool: PgPool, kind: ConnectionKind) -> Hook {
    let prefix = format!("test-{}", Uuid::new_v4());
    let queue = common::test_queue(&prefix).await;
    let connection = connections::insert(&pool, kind, "c", "org", "enc", "abcd", "owner")
        .await
        .unwrap();
    let repo = repositories::insert(
        &pool,
        NewRepository {
            connection_id: connection.id,
            external_id: "4bc14d40-c903-45e2-872e-0462c7748079",
            owner: "org",
            project: None,
            project_name: None,
            name: "Hello-World",
            default_branch: None,
            webhook_token: TOKEN,
        },
    )
    .await
    .unwrap()
    .unwrap();
    Hook {
        app: common::app_with_queue(pool.clone(), queue),
        pool,
        prefix,
        repo_id: repo.id,
    }
}

/// Semua job di stream antrian test, urut masuk.
async fn queued_jobs(prefix: &str) -> Vec<Job> {
    let client = redis::Client::open(common::redis_url()).unwrap();
    let mut conn = client.get_multiplexed_async_connection().await.unwrap();
    let entries: Vec<(String, HashMap<String, String>)> = redis::cmd("XRANGE")
        .arg(format!("{prefix}:jobs"))
        .arg("-")
        .arg("+")
        .query_async(&mut conn)
        .await
        .unwrap();
    entries
        .iter()
        .map(|(_, fields)| serde_json::from_str(&fields["job"]).unwrap())
        .collect()
}

async fn dedup_key_exists(prefix: &str, key: &str) -> bool {
    let client = redis::Client::open(common::redis_url()).unwrap();
    let mut conn = client.get_multiplexed_async_connection().await.unwrap();
    redis::cmd("EXISTS")
        .arg(format!("{prefix}:dedup:{key}"))
        .query_async(&mut conn)
        .await
        .unwrap()
}

/// Ubah fixture JSON lalu kembalikan sebagai teks.
fn edited(fixture: &str, edit: impl FnOnce(&mut Value)) -> String {
    let mut value: Value = serde_json::from_str(fixture).unwrap();
    edit(&mut value);
    value.to_string()
}

fn sign(secret: &str, body: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(body.as_bytes());
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

fn github_request(
    repo_id: Uuid,
    event: &str,
    delivery: &str,
    signature: &str,
    body: &str,
) -> Request<Body> {
    Request::post(format!("/webhooks/github/{repo_id}"))
        .header(header::CONTENT_TYPE, "application/json")
        .header("X-GitHub-Event", event)
        .header("X-GitHub-Delivery", delivery)
        .header("X-Hub-Signature-256", signature)
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// Request GitHub dengan signature yang benar untuk `TOKEN`.
fn github_signed(repo_id: Uuid, event: &str, delivery: &str, body: &str) -> Request<Body> {
    github_request(repo_id, event, delivery, &sign(TOKEN, body), body)
}

fn azure_request(repo_id: Uuid, token: &str, body: &str) -> Request<Body> {
    Request::post(format!("/webhooks/azure/{repo_id}?token={token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn status_body(text: &str) -> Value {
    json!({ "status": text })
}

#[sqlx::test(migrations = "../../migrations")]
async fn github_bad_signature_401(pool: PgPool) {
    let hook = setup(pool, ConnectionKind::Github).await;
    let wrong = sign("other-secret", GH_PR_OPENED);
    for signature in [wrong.as_str(), "sha256=zz", "", "sha1=abc"] {
        let req = github_request(hook.repo_id, "pull_request", "d-1", signature, GH_PR_OPENED);
        let reply = common::send(&hook.app, req).await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{signature}");
        assert_eq!(reply.body["error"]["code"], "invalid_signature");
    }
    // Header hilang sama sekali.
    let req = Request::post(format!("/webhooks/github/{}", hook.repo_id))
        .header("X-GitHub-Event", "pull_request")
        .body(Body::from(GH_PR_OPENED))
        .unwrap();
    assert_eq!(
        common::send(&hook.app, req).await.status,
        StatusCode::UNAUTHORIZED
    );
    assert!(queued_jobs(&hook.prefix).await.is_empty());
    common::cleanup_queue(&hook.prefix).await;
}

#[sqlx::test(migrations = "../../migrations")]
async fn github_pr_opened_enqueues_review(pool: PgPool) {
    let hook = setup(pool, ConnectionKind::Github).await;
    let req = github_signed(hook.repo_id, "pull_request", "d-1", GH_PR_OPENED);
    let reply = common::send(&hook.app, req).await;
    assert_eq!(reply.status, StatusCode::ACCEPTED);
    assert_eq!(reply.body, status_body("queued"));
    assert_eq!(
        queued_jobs(&hook.prefix).await,
        vec![Job::ReviewPr {
            repository_id: hook.repo_id,
            pr_number: 2,
            trigger: Trigger::Opened,
            mode_override: None,
        }]
    );
    common::cleanup_queue(&hook.prefix).await;
}

#[sqlx::test(migrations = "../../migrations")]
async fn github_draft_ready_for_review_enqueues(pool: PgPool) {
    let hook = setup(pool, ConnectionKind::Github).await;
    let body = edited(GH_PR_OPENED, |v| v["action"] = json!("ready_for_review"));
    let reply = common::send(
        &hook.app,
        github_signed(hook.repo_id, "pull_request", "d-1", &body),
    )
    .await;
    assert_eq!(reply.status, StatusCode::ACCEPTED);
    assert_eq!(
        queued_jobs(&hook.prefix).await,
        vec![Job::ReviewPr {
            repository_id: hook.repo_id,
            pr_number: 2,
            trigger: Trigger::Opened,
            mode_override: None,
        }]
    );
    common::cleanup_queue(&hook.prefix).await;
}

#[sqlx::test(migrations = "../../migrations")]
async fn github_duplicate_delivery_enqueues_once(pool: PgPool) {
    let hook = setup(pool, ConnectionKind::Github).await;
    let first = common::send(
        &hook.app,
        github_signed(hook.repo_id, "pull_request", "same-delivery", GH_PR_OPENED),
    )
    .await;
    assert_eq!(first.status, StatusCode::ACCEPTED);
    let again = common::send(
        &hook.app,
        github_signed(hook.repo_id, "pull_request", "same-delivery", GH_PR_OPENED),
    )
    .await;
    assert_eq!(again.status, StatusCode::OK);
    assert_eq!(again.body, status_body("duplicate"));
    assert_eq!(queued_jobs(&hook.prefix).await.len(), 1);
    // Key dedup per repo: delivery id yang sama di repo lain tidak bertabrakan.
    assert!(dedup_key_exists(&hook.prefix, &format!("gh:{}:same-delivery", hook.repo_id)).await);
    common::cleanup_queue(&hook.prefix).await;
}

#[sqlx::test(migrations = "../../migrations")]
async fn github_issue_comment_on_plain_issue_ignored(pool: PgPool) {
    let hook = setup(pool, ConnectionKind::Github).await;
    let req = github_signed(hook.repo_id, "issue_comment", "d-1", GH_PLAIN_ISSUE_COMMENT);
    let reply = common::send(&hook.app, req).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.body, status_body("ignored"));
    assert!(queued_jobs(&hook.prefix).await.is_empty());
    common::cleanup_queue(&hook.prefix).await;
}

#[sqlx::test(migrations = "../../migrations")]
async fn github_pr_comment_enqueues_comment_job(pool: PgPool) {
    let hook = setup(pool, ConnectionKind::Github).await;
    let req = github_signed(hook.repo_id, "issue_comment", "d-1", GH_ISSUE_COMMENT);
    assert_eq!(
        common::send(&hook.app, req).await.status,
        StatusCode::ACCEPTED
    );
    assert_eq!(
        queued_jobs(&hook.prefix).await,
        vec![Job::PrComment {
            repository_id: hook.repo_id,
            pr_number: 2,
            comment_url: Some(
                "https://github.com/Codertocat/Hello-World/pull/2#issuecomment-492700400".into()
            ),
            author: CommentAuthor {
                login: Some("Codertocat".into()),
                email: None,
            },
            body: "@adudu review".into(),
        }]
    );
    common::cleanup_queue(&hook.prefix).await;
}

#[sqlx::test(migrations = "../../migrations")]
async fn github_ping_ignored(pool: PgPool) {
    let hook = setup(pool, ConnectionKind::Github).await;
    let reply = common::send(
        &hook.app,
        github_signed(hook.repo_id, "ping", "d-1", GH_PING),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.body, status_body("ignored"));
    common::cleanup_queue(&hook.prefix).await;
}

#[sqlx::test(migrations = "../../migrations")]
async fn github_invalid_json_after_valid_signature_400(pool: PgPool) {
    let hook = setup(pool, ConnectionKind::Github).await;
    let req = github_signed(hook.repo_id, "pull_request", "d-1", "{not json");
    let reply = common::send(&hook.app, req).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(reply.body["error"]["code"], "bad_request");
    common::cleanup_queue(&hook.prefix).await;
}

#[sqlx::test(migrations = "../../migrations")]
async fn azure_wrong_token_401(pool: PgPool) {
    let hook = setup(pool, ConnectionKind::AzureDevops).await;
    let reply = common::send(
        &hook.app,
        azure_request(hook.repo_id, "nope", AZ_PR_CREATED),
    )
    .await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    assert_eq!(reply.body["error"]["code"], "invalid_signature");

    let missing = Request::post(format!("/webhooks/azure/{}", hook.repo_id))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(AZ_PR_CREATED))
        .unwrap();
    assert_eq!(
        common::send(&hook.app, missing).await.status,
        StatusCode::UNAUTHORIZED
    );
    assert!(queued_jobs(&hook.prefix).await.is_empty());
    common::cleanup_queue(&hook.prefix).await;
}

#[sqlx::test(migrations = "../../migrations")]
async fn azure_pr_created_enqueues(pool: PgPool) {
    let hook = setup(pool, ConnectionKind::AzureDevops).await;
    let reply = common::send(&hook.app, azure_request(hook.repo_id, TOKEN, AZ_PR_CREATED)).await;
    assert_eq!(reply.status, StatusCode::ACCEPTED);
    assert_eq!(reply.body, status_body("queued"));
    assert_eq!(
        queued_jobs(&hook.prefix).await,
        vec![Job::ReviewPr {
            repository_id: hook.repo_id,
            pr_number: 1,
            trigger: Trigger::Opened,
            mode_override: None,
        }]
    );
    // Notifikasi yang sama dikirim ulang: tidak di-enqueue lagi.
    let again = common::send(&hook.app, azure_request(hook.repo_id, TOKEN, AZ_PR_CREATED)).await;
    assert_eq!(again.body, status_body("duplicate"));
    assert_eq!(queued_jobs(&hook.prefix).await.len(), 1);
    common::cleanup_queue(&hook.prefix).await;
}

#[sqlx::test(migrations = "../../migrations")]
async fn azure_pr_updated_enqueues_updated_trigger(pool: PgPool) {
    let hook = setup(pool, ConnectionKind::AzureDevops).await;
    let reply = common::send(&hook.app, azure_request(hook.repo_id, TOKEN, AZ_PR_UPDATED)).await;
    assert_eq!(reply.status, StatusCode::ACCEPTED);
    assert_eq!(
        queued_jobs(&hook.prefix).await,
        vec![Job::ReviewPr {
            repository_id: hook.repo_id,
            pr_number: 1,
            trigger: Trigger::Updated,
            mode_override: None,
        }]
    );
    common::cleanup_queue(&hook.prefix).await;
}

#[sqlx::test(migrations = "../../migrations")]
async fn azure_closed_pr_queues_learning(pool: PgPool) {
    let hook = setup(pool, ConnectionKind::AzureDevops).await;
    for status in ["completed", "abandoned"] {
        // A distinct notification id per status, so delivery dedup does not drop the second.
        let body = edited(AZ_PR_UPDATED, |v| {
            v["resource"]["status"] = json!(status);
            v["id"] = json!(format!("closed-{status}"));
        });
        let reply = common::send(&hook.app, azure_request(hook.repo_id, TOKEN, &body)).await;
        assert_eq!(reply.status, StatusCode::ACCEPTED, "{status}");
        assert_eq!(reply.body, status_body("queued"));
    }
    let learn = Job::LearnPr {
        repository_id: hook.repo_id,
        pr_number: 1,
    };
    assert_eq!(queued_jobs(&hook.prefix).await, vec![learn.clone(), learn]);
    common::cleanup_queue(&hook.prefix).await;
}

#[sqlx::test(migrations = "../../migrations")]
async fn azure_comment_maps_unique_name_to_email(pool: PgPool) {
    let hook = setup(pool, ConnectionKind::AzureDevops).await;
    let reply = common::send(&hook.app, azure_request(hook.repo_id, TOKEN, AZ_PR_COMMENT)).await;
    assert_eq!(reply.status, StatusCode::ACCEPTED);
    assert_eq!(
        queued_jobs(&hook.prefix).await,
        vec![Job::PrComment {
            repository_id: hook.repo_id,
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
        }]
    );
    common::cleanup_queue(&hook.prefix).await;
}

#[sqlx::test(migrations = "../../migrations")]
async fn disabled_repo_ignored_200(pool: PgPool) {
    let hook = setup(pool, ConnectionKind::Github).await;
    let patch = RepoPatch {
        enabled: Some(false),
        ..RepoPatch::default()
    };
    repositories::update(&hook.pool, hook.repo_id, patch)
        .await
        .unwrap();

    let req = github_signed(hook.repo_id, "pull_request", "d-1", GH_PR_OPENED);
    let reply = common::send(&hook.app, req).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.body, status_body("ignored"));
    // Signature tetap diperiksa untuk repo nonaktif.
    let req = github_request(
        hook.repo_id,
        "pull_request",
        "d-2",
        "sha256=00",
        GH_PR_OPENED,
    );
    assert_eq!(
        common::send(&hook.app, req).await.status,
        StatusCode::UNAUTHORIZED
    );
    assert!(queued_jobs(&hook.prefix).await.is_empty());
    common::cleanup_queue(&hook.prefix).await;
}

#[sqlx::test(migrations = "../../migrations")]
async fn deleted_repo_404(pool: PgPool) {
    let hook = setup(pool, ConnectionKind::Github).await;
    repositories::soft_delete(&hook.pool, hook.repo_id)
        .await
        .unwrap();

    let req = github_signed(hook.repo_id, "pull_request", "d-1", GH_PR_OPENED);
    let reply = common::send(&hook.app, req).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert_eq!(reply.body["error"]["code"], "not_found");
    // Id yang tidak pernah ada dan path yang bukan UUID juga 404.
    let req = github_signed(Uuid::new_v4(), "pull_request", "d-2", GH_PR_OPENED);
    assert_eq!(
        common::send(&hook.app, req).await.status,
        StatusCode::NOT_FOUND
    );
    let req = Request::post("/webhooks/github/not-a-uuid")
        .body(Body::from(GH_PR_OPENED))
        .unwrap();
    assert_eq!(
        common::send(&hook.app, req).await.status,
        StatusCode::NOT_FOUND
    );
    assert!(queued_jobs(&hook.prefix).await.is_empty());
    common::cleanup_queue(&hook.prefix).await;
}

#[sqlx::test(migrations = "../../migrations")]
async fn wrong_provider_endpoint_404(pool: PgPool) {
    let hook = setup(pool, ConnectionKind::Github).await;
    let reply = common::send(&hook.app, azure_request(hook.repo_id, TOKEN, AZ_PR_CREATED)).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert!(queued_jobs(&hook.prefix).await.is_empty());
    common::cleanup_queue(&hook.prefix).await;
}

#[sqlx::test(migrations = "../../migrations")]
async fn own_marker_comment_ignored(pool: PgPool) {
    let hook = setup(pool, ConnectionKind::Github).await;
    let body = edited(GH_ISSUE_COMMENT, |v| {
        v["comment"]["body"] = json!("<!-- adudu -->\n🤖 **Adudu** reviewed this PR")
    });
    let reply = common::send(
        &hook.app,
        github_signed(hook.repo_id, "issue_comment", "d-1", &body),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.body, status_body("ignored"));
    assert!(queued_jobs(&hook.prefix).await.is_empty());
    common::cleanup_queue(&hook.prefix).await;
}

#[sqlx::test(migrations = "../../migrations")]
async fn webhook_body_limit_is_25mb(pool: PgPool) {
    let hook = setup(pool, ConnectionKind::Github).await;
    // ~3 MB: di atas batas default axum (2 MB), di bawah batas webhook.
    let big = edited(GH_PR_OPENED, |v| {
        v["pull_request"]["body"] = json!("x".repeat(3 * 1024 * 1024))
    });
    let reply = common::send(
        &hook.app,
        github_signed(hook.repo_id, "pull_request", "d-1", &big),
    )
    .await;
    assert_eq!(reply.status, StatusCode::ACCEPTED);

    let huge = "x".repeat(25 * 1024 * 1024 + 1);
    let reply = common::send(
        &hook.app,
        github_signed(hook.repo_id, "pull_request", "d-2", &huge),
    )
    .await;
    assert_eq!(reply.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(reply.body["error"]["code"], "payload_too_large");

    // Repo tidak ada: 404 sebelum body dibaca, walau body melebihi batas.
    let reply = common::send(
        &hook.app,
        github_signed(Uuid::new_v4(), "pull_request", "d-3", &huge),
    )
    .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    common::cleanup_queue(&hook.prefix).await;
}

#[sqlx::test(migrations = "../../migrations")]
async fn azure_token_checked_before_body_is_read(pool: PgPool) {
    let hook = setup(pool, ConnectionKind::AzureDevops).await;
    let huge = "x".repeat(25 * 1024 * 1024 + 1);
    let reply = common::send(&hook.app, azure_request(hook.repo_id, "nope", &huge)).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    let reply = common::send(&hook.app, azure_request(hook.repo_id, TOKEN, &huge)).await;
    assert_eq!(reply.status, StatusCode::PAYLOAD_TOO_LARGE);
    common::cleanup_queue(&hook.prefix).await;
}

#[sqlx::test(migrations = "../../migrations")]
async fn api_json_route_keeps_default_body_limit(pool: PgPool) {
    let app = common::app(pool).await;
    let body = json!({ "email": "a@b.test", "password": "x".repeat(3 * 1024 * 1024) });
    let reply = common::send(&app, common::post_json("/api/v1/auth/login", &body, None)).await;
    assert_eq!(reply.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(reply.body["error"]["code"], "payload_too_large");
}
