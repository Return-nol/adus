mod common;

use std::{sync::Arc, time::Duration};

use adus_core::{
    job::{Job, Trigger},
    types::{ConnectionKind, ProviderKind},
};
use adus_db::{
    ai,
    connections::{self, ConnectionStatus},
    repositories::{self, NewRepository},
    settings::{self, Settings},
};
use adus_server::{
    AppState,
    doctor::{self, Report, Status, Verdict, print_report},
};
use axum::http::StatusCode;
use common::{
    FakeGitFactory, app_state, app_with_queue, cleanup_queue, cookie_pair, get, post_json, send,
    test_queue,
};
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

const PAT: &str = "ghp_supersecret_token_9999";
const API_KEY: &str = "sk-live-supersecret-key-7777";

struct World {
    state: AppState,
    git: Arc<FakeGitFactory>,
    prefix: String,
}

/// Semua sehat: worker hidup, model default terisi, satu key, satu connection dengan satu repo.
async fn healthy_world(pool: PgPool) -> World {
    let prefix = format!("test-{}", Uuid::new_v4());
    let queue = test_queue(&prefix).await;
    queue.heartbeat("worker-1").await.unwrap();
    let git = Arc::new(FakeGitFactory::default());
    let state = app_state(pool.clone(), "http://localhost:8080", git.clone(), queue);

    let provider = ai::insert_provider(&pool, ProviderKind::Openai, "OpenAI", None, "gpt-x", None)
        .await
        .unwrap();
    let encrypted = state.cipher.encrypt(API_KEY);
    ai::insert_key(&pool, provider.id, "main key", &encrypted, "7777")
        .await
        .unwrap();
    settings::update(
        &pool,
        &Settings {
            default_provider_id: Some(provider.id),
            max_files_per_review: 50,
            max_patch_lines_per_file: 2000,
            ignore_paths: None,
            ignored_title_keywords: vec![],
            review_mode: Default::default(),
            small_pr_max_lines: 50,
        },
    )
    .await
    .unwrap();

    let encrypted = state.cipher.encrypt(PAT);
    let conn = connections::insert(
        &pool,
        ConnectionKind::Github,
        "Acme",
        "acme",
        &encrypted,
        "9999",
        "me",
    )
    .await
    .unwrap();
    let repo = repositories::insert(
        &pool,
        NewRepository {
            connection_id: conn.id,
            external_id: "r1",
            owner: "acme",
            project: None,
            project_name: None,
            name: "api",
            default_branch: None,
            webhook_token: "wh",
        },
    )
    .await
    .unwrap()
    .unwrap();
    repositories::set_webhook_ids(&pool, repo.id, &["hook-1".to_string()])
        .await
        .unwrap();
    World { state, git, prefix }
}

fn fails(report: &Report) -> Vec<&str> {
    report
        .checks
        .iter()
        .filter(|c| c.status == Status::Fail)
        .map(|c| c.name.as_str())
        .collect()
}

#[sqlx::test(migrations = "../../migrations")]
async fn doctor_ok_when_everything_is_healthy(pool: PgPool) {
    let w = healthy_world(pool).await;
    let report = doctor::run(&w.state).await;
    assert_eq!(report.verdict, Verdict::Ok);
    assert!(report.checks.iter().all(|c| c.status == Status::Ok));
    assert!(report.checks.len() >= 8, "{:?}", report.checks);
    cleanup_queue(&w.prefix).await;
}

#[sqlx::test(migrations = "../../migrations")]
async fn doctor_not_running_without_worker_heartbeat(pool: PgPool) {
    let w = healthy_world(pool).await;
    // Antrian dengan prefix lain tidak punya heartbeat worker.
    let prefix = format!("test-{}", Uuid::new_v4());
    let mut state = w.state.clone();
    state.queue = test_queue(&prefix).await;

    let report = doctor::run(&state).await;
    assert_eq!(report.verdict, Verdict::NotRunning);
    assert_eq!(fails(&report), vec!["Workers"]);
    let check = &report.checks[0];
    assert!(check.impact.is_some() && check.fix.is_some());
    cleanup_queue(&w.prefix).await;
    cleanup_queue(&prefix).await;
}

#[sqlx::test(migrations = "../../migrations")]
async fn doctor_not_running_without_default_provider(pool: PgPool) {
    let w = healthy_world(pool.clone()).await;
    let current = settings::get(&pool).await.unwrap();
    settings::update(
        &pool,
        &Settings {
            default_provider_id: None,
            ..current
        },
    )
    .await
    .unwrap();

    let report = doctor::run(&w.state).await;
    assert_eq!(report.verdict, Verdict::NotRunning);
    assert_eq!(fails(&report), vec!["Default provider"]);
    cleanup_queue(&w.prefix).await;
}

#[sqlx::test(migrations = "../../migrations")]
async fn doctor_not_running_when_default_provider_disabled(pool: PgPool) {
    let w = healthy_world(pool.clone()).await;
    let provider_id = settings::get(&pool)
        .await
        .unwrap()
        .default_provider_id
        .unwrap();
    ai::update_provider(
        &pool,
        provider_id,
        "OpenAI",
        None,
        Some("gpt-x"),
        None,
        false,
    )
    .await
    .unwrap();

    let report = doctor::run(&w.state).await;
    assert_eq!(report.verdict, Verdict::NotRunning);
    assert_eq!(fails(&report), vec!["Default provider", "AI key"]);
    cleanup_queue(&w.prefix).await;
}

#[sqlx::test(migrations = "../../migrations")]
async fn doctor_not_running_when_default_provider_has_no_model(pool: PgPool) {
    let w = healthy_world(pool.clone()).await;
    sqlx::query("UPDATE ai_providers SET primary_model = NULL")
        .execute(&pool)
        .await
        .unwrap();

    let report = doctor::run(&w.state).await;
    assert_eq!(report.verdict, Verdict::NotRunning);
    assert_eq!(fails(&report), vec!["Default provider"]);
    cleanup_queue(&w.prefix).await;
}

#[sqlx::test(migrations = "../../migrations")]
async fn doctor_not_running_without_usable_key(pool: PgPool) {
    let w = healthy_world(pool.clone()).await;
    sqlx::query("UPDATE ai_keys SET enabled = false")
        .execute(&pool)
        .await
        .unwrap();

    let report = doctor::run(&w.state).await;
    assert_eq!(report.verdict, Verdict::NotRunning);
    assert_eq!(fails(&report), vec!["AI key"]);
    cleanup_queue(&w.prefix).await;
}

#[sqlx::test(migrations = "../../migrations")]
async fn doctor_not_running_when_migration_missing(pool: PgPool) {
    let w = healthy_world(pool.clone()).await;
    sqlx::query(
        "DELETE FROM _sqlx_migrations \
         WHERE version = (SELECT max(version) FROM _sqlx_migrations)",
    )
    .execute(&pool)
    .await
    .unwrap();

    let report = doctor::run(&w.state).await;
    assert_eq!(report.verdict, Verdict::NotRunning);
    assert_eq!(fails(&report), vec!["Database migrations"]);
    cleanup_queue(&w.prefix).await;
}

#[sqlx::test(migrations = "../../migrations")]
async fn doctor_degraded_when_connection_token_invalid(pool: PgPool) {
    let w = healthy_world(pool.clone()).await;
    w.git.with(|s| {
        s.bad_tokens.insert(PAT.to_string());
    });

    let report = doctor::run(&w.state).await;
    assert_eq!(report.verdict, Verdict::Degraded);
    // Repo milik connection itu tidak diperiksa lagi, supaya tidak jadi kegagalan ganda.
    assert_eq!(fails(&report), vec!["Connection: Acme"]);
    // Doctor never writes the status: the connection stays `ok`.
    let conns = connections::list(&pool).await.unwrap();
    assert_eq!(conns[0].status, ConnectionStatus::Ok);
    cleanup_queue(&w.prefix).await;
}

#[sqlx::test(migrations = "../../migrations")]
async fn doctor_degraded_when_webhook_missing(pool: PgPool) {
    let w = healthy_world(pool).await;
    w.git.with(|s| {
        s.missing_webhooks.insert("r1".into());
    });

    let report = doctor::run(&w.state).await;
    assert_eq!(report.verdict, Verdict::Degraded);
    assert_eq!(fails(&report), vec!["Webhooks: acme/api"]);
    cleanup_queue(&w.prefix).await;
}

#[sqlx::test(migrations = "../../migrations")]
async fn doctor_degraded_when_webhook_points_to_old_public_url(pool: PgPool) {
    let w = healthy_world(pool).await;
    w.git.with(|s| {
        s.registered_webhook_url = Some("https://old.example/webhooks/github/x".into());
    });

    let report = doctor::run(&w.state).await;
    assert_eq!(report.verdict, Verdict::Degraded);
    assert_eq!(fails(&report), vec!["Webhooks: acme/api"]);
    cleanup_queue(&w.prefix).await;
}

#[sqlx::test(migrations = "../../migrations")]
async fn doctor_names_azure_webhook_check_by_project(pool: PgPool) {
    let w = healthy_world(pool.clone()).await;
    let encrypted = w.state.cipher.encrypt(PAT);
    let conn = connections::insert(
        &pool,
        ConnectionKind::AzureDevops,
        "Contoso",
        "contoso",
        &encrypted,
        "9999",
        "me",
    )
    .await
    .unwrap();
    let repo = repositories::insert(
        &pool,
        NewRepository {
            connection_id: conn.id,
            external_id: "az1",
            owner: "contoso",
            project: Some("p-guid"),
            project_name: Some("Platform"),
            name: "api",
            default_branch: None,
            webhook_token: "wh2",
        },
    )
    .await
    .unwrap()
    .unwrap();
    repositories::set_webhook_ids(&pool, repo.id, &["sub-1".to_string()])
        .await
        .unwrap();
    w.git.with(|s| {
        s.missing_webhooks.insert("az1".into());
    });

    let report = doctor::run(&w.state).await;
    assert_eq!(fails(&report), vec!["Webhooks: Platform/api"]);
    cleanup_queue(&w.prefix).await;
}

#[sqlx::test(migrations = "../../migrations")]
async fn doctor_never_calls_ensure_webhooks(pool: PgPool) {
    let w = healthy_world(pool.clone()).await;
    w.git.with(|s| {
        s.missing_webhooks.insert("r1".into());
    });
    doctor::run(&w.state).await;
    assert!(w.git.ensure_calls().is_empty());
    assert!(w.git.delete_calls().is_empty());
    // Dan tidak mengubah apa pun di DB.
    let conns = connections::list(&pool).await.unwrap();
    assert_eq!(conns[0].status, ConnectionStatus::Ok);
    cleanup_queue(&w.prefix).await;
}

#[sqlx::test(migrations = "../../migrations")]
async fn doctor_warns_on_dead_letters(pool: PgPool) {
    let w = healthy_world(pool).await;
    let job = Job::ReviewPr {
        repository_id: Uuid::new_v4(),
        pr_number: 1,
        trigger: Trigger::Opened,
        mode_override: None,
    };
    w.state.queue.enqueue(&job).await.unwrap();
    let delivery = w
        .state
        .queue
        .next("worker-1", Duration::from_millis(200))
        .await
        .unwrap()
        .unwrap();
    w.state.queue.dead_letter(&delivery, "boom").await.unwrap();

    let report = doctor::run(&w.state).await;
    // Warn tidak mengubah vonis, tetapi tampil sebelum cek yang sehat.
    assert_eq!(report.verdict, Verdict::Ok);
    assert_eq!(report.checks[0].status, Status::Warn);
    assert!(
        report.checks[0].name.contains("1 failed job"),
        "{}",
        report.checks[0].name
    );
    assert_eq!(
        report
            .checks
            .iter()
            .filter(|c| c.status == Status::Warn)
            .count(),
        1
    );
    cleanup_queue(&w.prefix).await;
}

#[sqlx::test(migrations = "../../migrations")]
async fn report_text_starts_with_verdict_and_has_no_secrets(pool: PgPool) {
    let w = healthy_world(pool).await;
    w.git.with(|s| {
        s.missing_webhooks.insert("r1".into());
    });
    let report = doctor::run(&w.state).await;
    let text = print_report(&report);
    assert!(text.starts_with("Reviews: DEGRADED\n"), "{text}");
    let broken = text.find('✘').expect("a failed check");
    let healthy = text.find('✓').expect("a healthy check");
    assert!(broken < healthy);
    assert!(text.contains("Webhooks: acme/api"));
    for secret in [PAT, API_KEY, "9999", "7777"] {
        assert!(!text.contains(secret), "leaked {secret}");
    }
    let json = serde_json::to_string(&report).unwrap();
    assert!(!json.contains(PAT) && !json.contains(API_KEY));
    cleanup_queue(&w.prefix).await;
}

#[sqlx::test(migrations = "../../migrations")]
async fn user_role_cannot_run_doctor_403(pool: PgPool) {
    let prefix = format!("test-{}", Uuid::new_v4());
    let app = app_with_queue(pool, test_queue(&prefix).await);
    let setup = json!({ "email": "admin@example.com", "password": "correct horse" });
    let admin = cookie_pair(&send(&app, post_json("/api/v1/auth/setup", &setup, None)).await);
    let created = json!({ "email": "u@example.com", "password": "correct horse", "role": "user" });
    send(&app, post_json("/api/v1/users", &created, Some(&admin))).await;
    let login = json!({ "email": "u@example.com", "password": "correct horse" });
    let user = cookie_pair(&send(&app, post_json("/api/v1/auth/login", &login, None)).await);

    let reply = send(&app, get("/api/v1/admin/doctor", Some(&user))).await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    let reply = send(&app, get("/api/v1/admin/doctor", None)).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);

    // Admin mendapat laporan JSON; tanpa worker dan model default, vonisnya not_running.
    let reply = send(&app, get("/api/v1/admin/doctor", Some(&admin))).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.body["verdict"], "not_running");
    let first = &reply.body["checks"][0];
    assert_eq!(first["status"], "fail");
    assert!(first["impact"].is_string() && first["fix"].is_string());
    cleanup_queue(&prefix).await;
}

#[sqlx::test(migrations = "../../migrations")]
async fn doctor_checks_the_git_binary(pool: PgPool) {
    let w = healthy_world(pool).await;

    let report = doctor::run(&w.state).await;

    // The test host (and CI) has git 2.31+ on PATH.
    let git: Vec<_> = report
        .checks
        .iter()
        .filter(|c| c.name.starts_with("Git"))
        .collect();
    assert_eq!(git.len(), 1, "{:?}", report.checks);
    assert_eq!(git[0].status, Status::Ok);
    assert!(
        git[0].name.starts_with("Git: git version 2."),
        "{}",
        git[0].name
    );
    cleanup_queue(&w.prefix).await;
}

#[tokio::test]
async fn missing_git_binary_is_a_warning_not_a_failure() {
    let check = doctor::git_binary_check("adus-no-such-git-binary").await;

    assert_eq!(check.status, Status::Warn);
    assert_eq!(check.name, "Git");
    assert_eq!(
        check.impact.as_deref(),
        Some(
            "`git` 2.31+ not found — reviews fall back to diff-only mode without codebase context"
        )
    );
    assert!(check.fix.is_some());
}
