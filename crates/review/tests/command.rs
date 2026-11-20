mod common;

use adus_core::job::{CommentAuthor, Job, Trigger};
use adus_core::types::{ADUDU_MARKER, ConnectionKind};
use adus_db::connections::{self, ConnectionStatus};
use adus_db::memories::{self, MemorySource};
use adus_db::rules::KnowledgeScope;
use adus_db::users::{self, Role, User, UserPatch};
use adus_git::GitError;
use adus_review::command::{Command, authorize, parse_command};
use adus_review::engine::EngineError;
use common::{FakeGit, Setup, cleanup_queue, pr, queued_jobs, setup, test_queue};
use sqlx::PgPool;

async fn user(pool: &PgPool, email: &str, github_login: Option<&str>) -> User {
    users::insert(pool, email, "x", Role::User, github_login)
        .await
        .unwrap()
}

fn author(login: Option<&str>, email: Option<&str>) -> CommentAuthor {
    CommentAuthor {
        login: login.map(str::to_string),
        email: email.map(str::to_string),
    }
}

/// Komentar di PR 7 repo hasil `setup`, dari GitHub user `octocat`.
fn comment(s: &Setup, body: &str) -> Job {
    Job::PrComment {
        repository_id: s.repository_id,
        pr_number: 7,
        comment_url: Some("https://github.com/acme/api/pull/7#issuecomment-1".into()),
        author: author(Some("octocat"), None),
        body: body.into(),
    }
}

async fn github_setup(pool: &PgPool) -> Setup {
    setup(pool, FakeGit::new(pr("h1"), Vec::new())).await
}

#[test]
fn parse_review_case_insensitive() {
    assert!(matches!(
        parse_command("@Adudu REVIEW"),
        Some(Command::Review)
    ));
    assert!(matches!(
        parse_command("thanks! @adudu review please"),
        Some(Command::Review)
    ));
}

#[test]
fn parse_remember_with_colon() {
    let Some(Command::Remember(text)) = parse_command("@adudu remember:  use anyhow in bins \n")
    else {
        panic!("expected Remember");
    };
    assert_eq!(text, "use anyhow in bins");
    let Some(Command::Remember(text)) = parse_command("@adudu Remember prefer tracing") else {
        panic!("expected Remember");
    };
    assert_eq!(text, "prefer tracing");
}

#[test]
fn parse_please_remember() {
    let Some(Command::Remember(text)) = parse_command("@adudu please remember: no unwrap") else {
        panic!("expected Remember");
    };
    assert_eq!(text, "no unwrap");
    let Some(Command::Remember(text)) = parse_command("@adudu PLEASE Remember tabs") else {
        panic!("expected Remember");
    };
    assert_eq!(text, "tabs");
}

#[test]
fn remember_empty_is_unknown() {
    assert!(matches!(
        parse_command("@adudu remember:   "),
        Some(Command::Unknown)
    ));
    assert!(matches!(
        parse_command("@adudu remember"),
        Some(Command::Unknown)
    ));
    assert!(matches!(
        parse_command("@adudu dance"),
        Some(Command::Unknown)
    ));
    assert!(matches!(parse_command("@adudu"), Some(Command::Unknown)));
    assert!(matches!(
        parse_command("@adudu reviewer"),
        Some(Command::Unknown)
    ));
}

#[test]
fn no_mention_is_none() {
    assert!(parse_command("please review this").is_none());
    assert!(parse_command("adudu review").is_none());
}

#[test]
fn adudu_prefix_word_is_none() {
    assert!(parse_command("@aduduku review").is_none());
}

#[test]
fn email_like_text_is_not_a_mention() {
    assert!(parse_command("mail foo@adudu.com review").is_none());
    // Hanya mention sungguhan yang dihitung, walau ada teks mirip sebelumnya.
    assert!(matches!(
        parse_command("foo@adudu.com — @adudu review"),
        Some(Command::Review)
    ));
}

#[test]
fn marker_comment_is_none() {
    let body = format!("{ADUDU_MARKER}\n@adudu review");
    assert!(parse_command(&body).is_none());
}

#[sqlx::test(migrations = "../../migrations")]
async fn azure_author_authorized_by_email(pool: PgPool) {
    let alice = user(&pool, "alice@acme.test", None).await;
    let found = authorize(
        &pool,
        ConnectionKind::AzureDevops,
        &author(Some("Alice@Acme.test"), Some("Alice@Acme.test")),
    )
    .await
    .unwrap();
    assert_eq!(found.map(|u| u.id), Some(alice.id));

    // Azure tidak mencocokkan github_login.
    let found = authorize(
        &pool,
        ConnectionKind::AzureDevops,
        &author(Some("alice"), None),
    )
    .await
    .unwrap();
    assert!(found.is_none());
}

#[sqlx::test(migrations = "../../migrations")]
async fn github_author_authorized_by_login(pool: PgPool) {
    let alice = user(&pool, "alice@acme.test", Some("alice-gh")).await;
    let bob = user(&pool, "bob@acme.test", None).await;

    let found = authorize(
        &pool,
        ConnectionKind::Github,
        &author(Some("Alice-GH"), None),
    )
    .await
    .unwrap();
    assert_eq!(found.map(|u| u.id), Some(alice.id));

    // Login tidak dikenal → coba email.
    let found = authorize(
        &pool,
        ConnectionKind::Github,
        &author(Some("bobby"), Some("bob@acme.test")),
    )
    .await
    .unwrap();
    assert_eq!(found.map(|u| u.id), Some(bob.id));

    let found = authorize(&pool, ConnectionKind::Github, &author(Some("eve"), None))
        .await
        .unwrap();
    assert!(found.is_none());
    let found = authorize(&pool, ConnectionKind::Github, &author(None, None))
        .await
        .unwrap();
    assert!(found.is_none());
}

#[sqlx::test(migrations = "../../migrations")]
async fn disabled_user_not_authorized(pool: PgPool) {
    let alice = user(&pool, "alice@acme.test", Some("alice")).await;
    let patch = UserPatch {
        disabled: Some(true),
        ..UserPatch::default()
    };
    users::update(&pool, alice.id, patch).await.unwrap();

    let found = authorize(&pool, ConnectionKind::Github, &author(Some("alice"), None))
        .await
        .unwrap();
    assert!(found.is_none());
    let found = authorize(
        &pool,
        ConnectionKind::AzureDevops,
        &author(None, Some("alice@acme.test")),
    )
    .await
    .unwrap();
    assert!(found.is_none());
}

#[sqlx::test(migrations = "../../migrations")]
async fn unregistered_author_gets_no_reply(pool: PgPool) {
    let s = github_setup(&pool).await;
    let (queue, prefix) = test_queue().await;

    let result = s
        .engine
        .handle_comment(&queue, &comment(&s, "@adudu review"))
        .await;

    assert_eq!(result, Ok(()));
    assert!(s.git.state().comments.is_empty());
    assert!(queued_jobs(&prefix).await.is_empty());
    cleanup_queue(&prefix).await;
}

#[sqlx::test(migrations = "../../migrations")]
async fn remember_creates_repo_memory_and_replies(pool: PgPool) {
    let s = github_setup(&pool).await;
    let octocat = user(&pool, "octo@acme.test", Some("octocat")).await;
    let (queue, prefix) = test_queue().await;

    let body = "@adudu remember: Use `thiserror` for library errors.\nNever `anyhow`.";
    let result = s.engine.handle_comment(&queue, &comment(&s, body)).await;

    assert_eq!(result, Ok(()));
    let saved = memories::list(&pool, Some(s.repository_id)).await.unwrap();
    assert_eq!(saved.len(), 1);
    let memory = &saved[0];
    assert_eq!(memory.scope, KnowledgeScope::Repo);
    assert_eq!(memory.repository_id, Some(s.repository_id));
    assert_eq!(
        memory.content,
        "Use `thiserror` for library errors.\nNever `anyhow`."
    );
    assert_eq!(memory.source, MemorySource::PrComment);
    assert_eq!(
        memory.source_url.as_deref(),
        Some("https://github.com/acme/api/pull/7#issuecomment-1")
    );
    assert_eq!(memory.created_by, Some(octocat.id));
    assert!(memory.enabled);

    let comments = s.git.state().comments.clone();
    assert_eq!(comments.len(), 1);
    assert!(comments[0].starts_with(ADUDU_MARKER));
    assert!(comments[0].contains("> Use `thiserror` for library errors.\n> Never `anyhow`."));
    assert!(queued_jobs(&prefix).await.is_empty());
    cleanup_queue(&prefix).await;
}

#[sqlx::test(migrations = "../../migrations")]
async fn remember_drops_non_http_source_url(pool: PgPool) {
    let s = github_setup(&pool).await;
    user(&pool, "octo@acme.test", Some("octocat")).await;
    let (queue, prefix) = test_queue().await;
    let mut job = comment(&s, "@adudu remember: x");
    if let Job::PrComment { comment_url, .. } = &mut job {
        *comment_url = Some("javascript:alert(1)".into());
    }

    s.engine.handle_comment(&queue, &job).await.unwrap();

    let saved = memories::list(&pool, Some(s.repository_id)).await.unwrap();
    assert_eq!(saved[0].source_url, None);
    cleanup_queue(&prefix).await;
}

#[sqlx::test(migrations = "../../migrations")]
async fn remember_too_long_replies_error(pool: PgPool) {
    let s = github_setup(&pool).await;
    user(&pool, "octo@acme.test", Some("octocat")).await;
    let (queue, prefix) = test_queue().await;
    let body = format!("@adudu remember: {}", "x".repeat(2001));

    let result = s.engine.handle_comment(&queue, &comment(&s, &body)).await;

    assert_eq!(result, Ok(()));
    assert!(
        memories::list(&pool, Some(s.repository_id))
            .await
            .unwrap()
            .is_empty()
    );
    let comments = s.git.state().comments.clone();
    assert_eq!(comments.len(), 1);
    assert!(comments[0].starts_with(ADUDU_MARKER));
    assert!(comments[0].contains("Memory is too long (max 2000 characters)."));

    // Tepat 2000 karakter masih boleh.
    let body = format!("@adudu remember: {}", "é".repeat(2000));
    s.engine
        .handle_comment(&queue, &comment(&s, &body))
        .await
        .unwrap();
    let saved = memories::list(&pool, Some(s.repository_id)).await.unwrap();
    assert_eq!(saved.len(), 1);
    cleanup_queue(&prefix).await;
}

#[sqlx::test(migrations = "../../migrations")]
async fn remember_reply_failure_does_not_retry(pool: PgPool) {
    let s = github_setup(&pool).await;
    user(&pool, "octo@acme.test", Some("octocat")).await;
    let (queue, prefix) = test_queue().await;
    s.git.state().comment_error = Some(GitError::RateLimited);

    let result = s
        .engine
        .handle_comment(&queue, &comment(&s, "@adudu remember: x"))
        .await;

    assert_eq!(result, Ok(()));
    let saved = memories::list(&pool, Some(s.repository_id)).await.unwrap();
    assert_eq!(saved.len(), 1);
    cleanup_queue(&prefix).await;
}

#[sqlx::test(migrations = "../../migrations")]
async fn review_command_enqueues_job(pool: PgPool) {
    let s = github_setup(&pool).await;
    user(&pool, "octo@acme.test", Some("octocat")).await;
    let (queue, prefix) = test_queue().await;

    let result = s
        .engine
        .handle_comment(&queue, &comment(&s, "@adudu review"))
        .await;

    assert_eq!(result, Ok(()));
    assert_eq!(
        queued_jobs(&prefix).await,
        vec![Job::ReviewPr {
            repository_id: s.repository_id,
            pr_number: 7,
            trigger: Trigger::Command,
        }]
    );
    let comments = s.git.state().comments.clone();
    assert_eq!(comments.len(), 1);
    assert!(comments[0].starts_with(ADUDU_MARKER));
    assert!(comments[0].contains("On it — starting a review."));

    // Enqueue sudah berhasil: gagal membalas tidak boleh membuat job di-retry.
    s.git.state().comment_error = Some(GitError::Http("boom".into()));
    let result = s
        .engine
        .handle_comment(&queue, &comment(&s, "@adudu review"))
        .await;
    assert_eq!(result, Ok(()));
    assert_eq!(queued_jobs(&prefix).await.len(), 2);
    cleanup_queue(&prefix).await;
}

#[sqlx::test(migrations = "../../migrations")]
async fn unknown_command_lists_commands(pool: PgPool) {
    let s = github_setup(&pool).await;
    user(&pool, "octo@acme.test", Some("octocat")).await;
    let (queue, prefix) = test_queue().await;

    let result = s
        .engine
        .handle_comment(&queue, &comment(&s, "@adudu dance"))
        .await;

    assert_eq!(result, Ok(()));
    let comments = s.git.state().comments.clone();
    assert_eq!(comments.len(), 1);
    assert!(comments[0].starts_with(ADUDU_MARKER));
    assert!(comments[0].contains("@adudu review"));
    assert!(comments[0].contains("@adudu remember:"));

    // Belum ada efek samping: gagal membalas boleh di-retry; token ditolak → fatal.
    s.git.state().comment_error = Some(GitError::RateLimited);
    let result = s
        .engine
        .handle_comment(&queue, &comment(&s, "@adudu dance"))
        .await;
    assert!(matches!(result, Err(EngineError::Retryable(..))));
    s.git.state().comment_error = Some(GitError::Unauthorized);
    let result = s
        .engine
        .handle_comment(&queue, &comment(&s, "@adudu dance"))
        .await;
    assert!(matches!(result, Err(EngineError::Fatal(_))));
    let conn = connections::get(&pool, s.connection_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(conn.status, ConnectionStatus::Invalid);
    cleanup_queue(&prefix).await;
}

#[sqlx::test(migrations = "../../migrations")]
async fn non_comment_job_is_fatal(pool: PgPool) {
    let s = github_setup(&pool).await;
    let (queue, prefix) = test_queue().await;
    let job = Job::ReviewPr {
        repository_id: s.repository_id,
        pr_number: 7,
        trigger: Trigger::Command,
    };

    let result = s.engine.handle_comment(&queue, &job).await;

    assert_eq!(result, Err(EngineError::Fatal("not a comment job".into())));
    cleanup_queue(&prefix).await;
}
