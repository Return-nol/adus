mod common;

use std::collections::HashMap;
use std::ops::Deref;
use std::sync::Arc;

use adus_core::job::{CommentAuthor, Job, Trigger};
use adus_core::types::{ADUDU_MARKER, ConnectionKind, ReviewMode};
use adus_db::connections::ConnectionStatus;
use adus_db::memories::{self, MemorySource};
use adus_db::rules::KnowledgeScope;
use adus_db::users::{self, Role, User, UserPatch};
use adus_git::{CloneSpec, GitError};
use adus_llm::router::{ChatError, ChatService, UsageContext};
use adus_llm::{ChatRequest, ChatResponse};
use adus_review::command::authorize;
use adus_review::engine::EngineError;
use adus_review::workspace::{RepoCloner, RepoWorkspace, WorkspaceError};
use async_trait::async_trait;
use common::{FakeGit, Setup, conn_status, pr, setup};
use sqlx::PgPool;
use uuid::Uuid;

struct NoopChat;

#[async_trait]
impl ChatService for NoopChat {
    async fn chat(
        &self,
        _provider_id: Uuid,
        _req: ChatRequest,
        _ctx: UsageContext,
    ) -> Result<ChatResponse, ChatError> {
        unreachable!("command tests do not invoke LLM")
    }
}

struct NoopCloner;

#[async_trait]
impl RepoCloner for NoopCloner {
    async fn clone_pr(&self, _spec: &CloneSpec) -> Result<RepoWorkspace, WorkspaceError> {
        unreachable!("command tests do not clone")
    }
}

fn redis_url() -> String {
    std::env::var("REDIS_URL").expect("REDIS_URL must be set (see .cargo/config.toml)")
}

/// Queue with a unique `test-{uuid}` prefix whose Redis keys are removed on drop, even on panic.
struct TestQueue {
    queue: adus_queue::Queue,
    prefix: String,
}

impl TestQueue {
    async fn new() -> Self {
        let prefix = format!("test-{}", Uuid::new_v4());
        let queue = adus_queue::Queue::connect(&redis_url(), &prefix)
            .await
            .unwrap();
        Self { queue, prefix }
    }

    async fn jobs(&self) -> Vec<Job> {
        let client = redis::Client::open(redis_url()).unwrap();
        let mut conn = client.get_multiplexed_async_connection().await.unwrap();
        let entries: Vec<(String, HashMap<String, String>)> = redis::cmd("XRANGE")
            .arg(format!("{}:jobs", self.prefix))
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
}

impl Deref for TestQueue {
    type Target = adus_queue::Queue;

    fn deref(&self) -> &Self::Target {
        &self.queue
    }
}

impl Drop for TestQueue {
    fn drop(&mut self) {
        use redis::Commands;
        let Ok(client) = redis::Client::open(redis_url()) else {
            return;
        };
        let Ok(mut conn) = client.get_connection() else {
            return;
        };
        let Ok(keys): Result<Vec<String>, _> = conn.keys(format!("{}:*", self.prefix)) else {
            return;
        };
        if !keys.is_empty() {
            let _: Result<(), _> = conn.del(keys);
        }
    }
}

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
    setup(
        pool,
        FakeGit::new(pr("h1"), Vec::new()),
        Arc::new(NoopChat),
        Arc::new(NoopCloner),
    )
    .await
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
    let queue = TestQueue::new().await;

    let result = s
        .engine
        .handle_comment(&queue, &comment(&s, "@adudu review"))
        .await;

    assert_eq!(result, Ok(()));
    assert!(s.git.state().comments.is_empty());
    assert!(queue.jobs().await.is_empty());
}

#[sqlx::test(migrations = "../../migrations")]
async fn remember_creates_repo_memory_and_replies(pool: PgPool) {
    let s = github_setup(&pool).await;
    let octocat = user(&pool, "octo@acme.test", Some("octocat")).await;
    let queue = TestQueue::new().await;

    let body = "@adudu remember: Use `thiserror` for library errors.\nNever `anyhow`.";
    let result = s.engine.handle_comment(&queue, &comment(&s, body)).await;

    assert_eq!(result, Ok(()));
    let saved = memories::list(&pool, Some(s.repository_id), None)
        .await
        .unwrap();
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
    assert!(queue.jobs().await.is_empty());

    // Non-http(s) comment_url is dropped from source_url.
    let mut non_http = comment(&s, "@adudu remember: drop js url");
    if let Job::PrComment { comment_url, .. } = &mut non_http {
        *comment_url = Some("javascript:alert(1)".into());
    }
    s.engine.handle_comment(&queue, &non_http).await.unwrap();
    let saved = memories::list(&pool, Some(s.repository_id), None)
        .await
        .unwrap();
    assert_eq!(saved.len(), 2);
    let non_http_mem = saved
        .iter()
        .find(|m| m.content == "drop js url")
        .expect("saved memory");
    assert_eq!(non_http_mem.source_url, None);

    // Memory was already saved: a reply failure does not retry.
    s.git.state().comment_error = Some(GitError::RateLimited);
    let result = s
        .engine
        .handle_comment(&queue, &comment(&s, "@adudu remember: after rate limit"))
        .await;
    assert_eq!(result, Ok(()));
    assert_eq!(
        memories::list(&pool, Some(s.repository_id), None)
            .await
            .unwrap()
            .len(),
        3
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn remember_too_long_replies_error(pool: PgPool) {
    let s = github_setup(&pool).await;
    user(&pool, "octo@acme.test", Some("octocat")).await;
    let queue = TestQueue::new().await;
    let body = format!("@adudu remember: {}", "x".repeat(2001));

    let result = s.engine.handle_comment(&queue, &comment(&s, &body)).await;

    assert_eq!(result, Ok(()));
    assert!(
        memories::list(&pool, Some(s.repository_id), None)
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
    let saved = memories::list(&pool, Some(s.repository_id), None)
        .await
        .unwrap();
    assert_eq!(saved.len(), 1);
}

#[sqlx::test(migrations = "../../migrations")]
async fn review_command_enqueues_job(pool: PgPool) {
    let s = github_setup(&pool).await;
    user(&pool, "octo@acme.test", Some("octocat")).await;
    let queue = TestQueue::new().await;

    let result = s
        .engine
        .handle_comment(&queue, &comment(&s, "@adudu review"))
        .await;

    assert_eq!(result, Ok(()));
    assert_eq!(
        queue.jobs().await,
        vec![Job::ReviewPr {
            repository_id: s.repository_id,
            pr_number: 7,
            trigger: Trigger::Command,
            mode_override: None,
        }]
    );
    let comments = s.git.state().comments.clone();
    assert_eq!(comments.len(), 1);
    assert!(comments[0].starts_with(ADUDU_MARKER));
    assert!(comments[0].contains("On it — starting a review."));

    // `@adudu review deep` enqueues with Specialists mode override.
    let result = s
        .engine
        .handle_comment(&queue, &comment(&s, "@adudu review deep"))
        .await;
    assert_eq!(result, Ok(()));
    assert_eq!(
        queue.jobs().await,
        vec![
            Job::ReviewPr {
                repository_id: s.repository_id,
                pr_number: 7,
                trigger: Trigger::Command,
                mode_override: None,
            },
            Job::ReviewPr {
                repository_id: s.repository_id,
                pr_number: 7,
                trigger: Trigger::Command,
                mode_override: Some(ReviewMode::Specialists),
            },
        ]
    );

    // Enqueue sudah berhasil: gagal membalas tidak boleh membuat job di-retry.
    s.git.state().comment_error = Some(GitError::Http("boom".into()));
    let result = s
        .engine
        .handle_comment(&queue, &comment(&s, "@adudu review"))
        .await;
    assert_eq!(result, Ok(()));
    assert_eq!(queue.jobs().await.len(), 3);
}

#[sqlx::test(migrations = "../../migrations")]
async fn unknown_command_lists_commands(pool: PgPool) {
    let s = github_setup(&pool).await;
    user(&pool, "octo@acme.test", Some("octocat")).await;
    let queue = TestQueue::new().await;

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
    assert_eq!(
        conn_status(&pool, s.connection_id).await,
        ConnectionStatus::Invalid
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn non_comment_job_is_fatal(pool: PgPool) {
    let s = github_setup(&pool).await;
    let queue = TestQueue::new().await;
    let job = Job::ReviewPr {
        repository_id: s.repository_id,
        pr_number: 7,
        trigger: Trigger::Command,
        mode_override: None,
    };

    let result = s.engine.handle_comment(&queue, &job).await;

    assert_eq!(result, Err(EngineError::Fatal("not a comment job".into())));
}
