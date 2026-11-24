// Not every harness helper is used here.
#[allow(dead_code)]
mod common;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use adus_core::types::{LearningMode, ProviderKind};
use adus_db::memories::{self, Memory, MemorySource, MemoryStatus, NewMemory};
use adus_db::repositories::{self, RepoPatch};
use adus_db::rules::KnowledgeScope;
use adus_db::users::{self, Role, UserPatch};
use adus_db::{ai, settings};
use adus_git::{CloneSpec, PrThread, PrThreadComment};
use adus_llm::router::{ChatError, ChatService, UsageContext};
use adus_llm::{ChatRequest, ChatResponse, Message, ReasoningEffort, StopReason, UserPart};
use adus_review::workspace::{RepoCloner, RepoWorkspace, WorkspaceError};
use async_trait::async_trait;
use common::{FakeGit, Setup, pr};
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

/// Answers with the scripted texts in order and records every request.
#[derive(Default)]
struct FakeChat {
    replies: Mutex<VecDeque<Result<String, ChatError>>>,
    requests: Mutex<Vec<(ChatRequest, UsageContext)>>,
}

impl FakeChat {
    fn reply(&self, text: &str) {
        self.replies.lock().unwrap().push_back(Ok(text.into()));
    }

    fn requests(&self) -> Vec<(ChatRequest, UsageContext)> {
        self.requests.lock().unwrap().clone()
    }

    fn prompt(&self) -> String {
        let requests = self.requests();
        let (req, _) = requests.first().expect("one LLM call");
        match &req.messages[0] {
            Message::User { parts } => match &parts[0] {
                UserPart::Text(text) => text.clone(),
            },
            other => panic!("unexpected message {other:?}"),
        }
    }
}

#[async_trait]
impl ChatService for FakeChat {
    async fn chat(
        &self,
        _provider_id: Uuid,
        req: ChatRequest,
        ctx: UsageContext,
    ) -> Result<ChatResponse, ChatError> {
        self.requests.lock().unwrap().push((req, ctx));
        let text = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("unscripted LLM call")?;
        Ok(ChatResponse {
            text,
            tool_calls: Vec::new(),
            stop_reason: StopReason::EndTurn,
            input_tokens: 10,
            output_tokens: 10,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            provider_meta: None,
        })
    }
}

struct NoopCloner;

#[async_trait]
impl RepoCloner for NoopCloner {
    async fn clone_pr(&self, _spec: &CloneSpec) -> Result<RepoWorkspace, WorkspaceError> {
        unreachable!("learning never clones")
    }
}

/// Engine over a GitHub repo in `mode`, with a provider whose fast model is `gpt-fast`.
async fn setup(
    pool: &PgPool,
    mode: LearningMode,
    threads: Vec<PrThread>,
) -> (Setup, Arc<FakeChat>) {
    let chat = Arc::new(FakeChat::default());
    let s = common::setup(
        pool,
        FakeGit::new(pr("h1"), Vec::new()),
        chat.clone(),
        Arc::new(NoopCloner),
    )
    .await;
    s.git.state().threads = threads;
    let provider = ai::insert_provider(
        pool,
        ProviderKind::Openai,
        "openai",
        None,
        "gpt-test",
        Some("gpt-fast"),
    )
    .await
    .unwrap();
    let mut current = settings::get(pool).await.unwrap();
    current.default_provider_id = Some(provider.id);
    settings::update(pool, &current).await.unwrap();
    let patch = RepoPatch {
        learning_mode: Some(mode),
        ..RepoPatch::default()
    };
    repositories::update(pool, s.repository_id, patch)
        .await
        .unwrap();
    (s, chat)
}

fn comment(author: &str, by_adus: bool, body: &str) -> PrThreadComment {
    PrThreadComment {
        author: author.into(),
        by_adus,
        body: body.into(),
    }
}

/// Adus flags something and `defender` answers that it is intentional.
fn rebutted(defender: &str, url: &str) -> PrThread {
    PrThread {
        path: Some("src/a.rs".into()),
        line: Some(3),
        status: None,
        url: Some(url.into()),
        comments: vec![
            comment("adudu-bot", true, "Retry loop has no backoff."),
            comment(
                defender,
                false,
                "Intentional: the PRD requires immediate retries.",
            ),
        ],
    }
}

fn answer(exceptions: serde_json::Value) -> String {
    json!({ "exceptions": exceptions }).to_string()
}

async fn learned(pool: &PgPool, repository_id: Uuid) -> Vec<Memory> {
    memories::for_learning(pool, repository_id)
        .await
        .unwrap()
        .into_iter()
        .filter(|m| m.source == MemorySource::PrLearning)
        .collect()
}

/// `(outcome, learned_count)` of every learning run.
async fn runs(pool: &PgPool) -> Vec<(String, i32)> {
    sqlx::query_as("SELECT outcome, learned_count FROM pr_learning_runs ORDER BY created_at")
        .fetch_all(pool)
        .await
        .unwrap()
}

#[sqlx::test(migrations = "../../migrations")]
async fn learn_pr_approval_mode_creates_pending_memory(pool: PgPool) {
    let url = "https://github.com/acme/api/pull/7#discussion_r1";
    let (s, chat) = setup(
        &pool,
        LearningMode::Approval,
        vec![rebutted("octocat", url)],
    )
    .await;
    chat.reply(&answer(json!([{
        "content": "Retries without backoff are intentional (PRD).",
        "thread": 0,
        "defended_by": [1],
    }])));

    s.engine.learn_pr(s.repository_id, 7).await.unwrap();

    let memories = learned(&pool, s.repository_id).await;
    assert_eq!(memories.len(), 1);
    let memory = &memories[0];
    assert_eq!(memory.status, MemoryStatus::Pending);
    assert_eq!(memory.scope, KnowledgeScope::Repo);
    assert_eq!(memory.repository_id, Some(s.repository_id));
    assert_eq!(
        memory.content,
        "Retries without backoff are intentional (PRD)."
    );
    assert_eq!(memory.source_url.as_deref(), Some(url));
    assert_eq!(memory.created_by, None);
    assert_eq!(runs(&pool).await, [("learned".to_string(), 1)]);

    let requests = chat.requests();
    let (req, ctx) = &requests[0];
    assert_eq!(req.model, "gpt-fast");
    assert_eq!(req.effort, Some(ReasoningEffort::Low));
    assert_eq!(ctx.review_id, None);
    assert_eq!(ctx.repository_id, Some(s.repository_id));
}

#[sqlx::test(migrations = "../../migrations")]
async fn learn_pr_auto_mode_activates_only_for_registered_defender(pool: PgPool) {
    // Thread 2: the registered defender answers, but an unregistered commenter took part too.
    let mut joined = rebutted("octocat", "https://example.test/3");
    joined.comments.insert(
        1,
        comment(
            "stranger",
            false,
            "By design, see the PRD, do not flag this.",
        ),
    );
    let threads = vec![
        rebutted("Octocat", "https://example.test/1"),
        rebutted("stranger", "https://example.test/2"),
        joined,
    ];
    let (s, chat) = setup(&pool, LearningMode::Auto, threads).await;
    let user = users::insert(&pool, "octo@example.test", "x", Role::User, Some("octocat"))
        .await
        .unwrap();
    chat.reply(&answer(json!([
        {"content": "A", "thread": 0, "defended_by": [1]},
        {"content": "B", "thread": 1, "defended_by": [1]},
        {"content": "C", "thread": 2, "defended_by": [2]},
        {"content": "invalid thread", "thread": 9, "defended_by": [1]},
        {"content": "invalid comment", "thread": 0, "defended_by": [5]},
        {"content": "  ", "thread": 0, "defended_by": [1]},
    ])));

    s.engine.learn_pr(s.repository_id, 7).await.unwrap();

    let memories = learned(&pool, s.repository_id).await;
    let summary: Vec<_> = memories
        .iter()
        .map(|m| (m.content.as_str(), m.status, m.created_by))
        .collect();
    assert_eq!(
        summary,
        [
            ("A", MemoryStatus::Active, Some(user.id)),
            ("B", MemoryStatus::Pending, None),
            ("C", MemoryStatus::Pending, None),
        ]
    );
    assert_eq!(runs(&pool).await, [("learned".to_string(), 3)]);
}

#[sqlx::test(migrations = "../../migrations")]
async fn learn_pr_auto_mode_disabled_user_stays_pending(pool: PgPool) {
    let threads = vec![rebutted("octocat", "https://example.test/1")];
    let (s, chat) = setup(&pool, LearningMode::Auto, threads).await;
    let user = users::insert(&pool, "octo@example.test", "x", Role::User, Some("octocat"))
        .await
        .unwrap();
    let patch = UserPatch {
        disabled: Some(true),
        ..UserPatch::default()
    };
    users::update(&pool, user.id, patch).await.unwrap();
    chat.reply(&answer(
        json!([{"content": "A", "thread": 0, "defended_by": [1]}]),
    ));

    s.engine.learn_pr(s.repository_id, 7).await.unwrap();

    let memories = learned(&pool, s.repository_id).await;
    assert_eq!(memories.len(), 1);
    assert_eq!(memories[0].status, MemoryStatus::Pending);
    assert_eq!(memories[0].created_by, None);
}

#[sqlx::test(migrations = "../../migrations")]
async fn learn_pr_runs_once_per_head(pool: PgPool) {
    let threads = vec![rebutted("octocat", "https://example.test/1")];
    let (s, chat) = setup(&pool, LearningMode::Approval, threads).await;
    chat.reply(&answer(
        json!([{"content": "A", "thread": 0, "defended_by": [1]}]),
    ));

    s.engine.learn_pr(s.repository_id, 7).await.unwrap();
    s.engine.learn_pr(s.repository_id, 7).await.unwrap();

    assert_eq!(chat.requests().len(), 1);
    assert_eq!(learned(&pool, s.repository_id).await.len(), 1);
    assert_eq!(runs(&pool).await.len(), 1);
}

#[sqlx::test(migrations = "../../migrations")]
async fn learn_pr_without_qualifying_threads_makes_no_llm_call(pool: PgPool) {
    let monologue = PrThread {
        comments: vec![
            comment("octocat", false, "Note to self."),
            comment("octocat", false, "This is intentional."),
        ],
        ..rebutted("octocat", "https://example.test/1")
    };
    let (s, chat) = setup(&pool, LearningMode::Approval, vec![monologue]).await;

    s.engine.learn_pr(s.repository_id, 7).await.unwrap();

    assert!(chat.requests().is_empty());
    assert_eq!(runs(&pool).await, [("nothing_to_learn".to_string(), 0)]);
}

#[sqlx::test(migrations = "../../migrations")]
async fn learn_pr_off_mode_does_nothing(pool: PgPool) {
    let threads = vec![rebutted("octocat", "https://example.test/1")];
    let (s, chat) = setup(&pool, LearningMode::Off, threads).await;

    s.engine.learn_pr(s.repository_id, 7).await.unwrap();

    assert!(chat.requests().is_empty());
    assert!(runs(&pool).await.is_empty());
}

#[sqlx::test(migrations = "../../migrations")]
async fn learn_pr_prompt_lists_rejected_and_redacts_secrets(pool: PgPool) {
    let secret = "ghp_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let mut thread = rebutted("octocat", "https://example.test/1");
    thread.comments[1].body = format!("Intentional; the test token {secret} is a fixture.");
    let (s, chat) = setup(&pool, LearningMode::Approval, vec![thread]).await;
    // Eleven older 2,000-char memories overflow the 20,000-char list: the oldest is cut.
    let fillers: Vec<String> = (0..11)
        .map(|i| format!("old-{i:02} {}", "x".repeat(1_993)))
        .collect();
    let mut inserts: Vec<(&str, MemoryStatus)> = fillers
        .iter()
        .map(|f| (f.as_str(), MemoryStatus::Active))
        .collect();
    inserts.push(("Known pending exception", MemoryStatus::Pending));
    inserts.push(("Rejected exception", MemoryStatus::Rejected));
    for (content, status) in inserts {
        let new = NewMemory {
            scope: KnowledgeScope::Repo,
            repository_id: Some(s.repository_id),
            content,
            source: MemorySource::PrLearning,
            source_url: None,
            created_by: None,
            status,
        };
        memories::insert(&pool, new).await.unwrap();
    }
    chat.reply(&answer(json!([])));

    s.engine.learn_pr(s.repository_id, 7).await.unwrap();

    let prompt = chat.prompt();
    assert!(!prompt.contains(secret));
    assert!(prompt.contains("[REDACTED]"));
    let known = prompt.find("<known_memories>").unwrap();
    let rejected = prompt.find("<rejected_memories>").unwrap();
    let known_list = &prompt[known..rejected];
    assert!(known_list.contains("Known pending exception"));
    assert!(!known_list.contains("Rejected exception"));
    assert!(known_list.contains("old-10 "), "newest filler kept");
    assert!(!known_list.contains("old-00 "), "oldest filler cut");
    assert!(known_list.chars().count() < 20_000 + 100);
    assert!(prompt[rejected..].contains("Rejected exception"));
    assert_eq!(runs(&pool).await, [("nothing_to_learn".to_string(), 0)]);
}

#[sqlx::test(migrations = "../../migrations")]
async fn learn_pr_invalid_answer_records_failed(pool: PgPool) {
    let threads = vec![rebutted("octocat", "https://example.test/1")];
    let (s, chat) = setup(&pool, LearningMode::Approval, threads).await;
    chat.reply("I could not decide.");

    s.engine.learn_pr(s.repository_id, 7).await.unwrap();

    assert!(learned(&pool, s.repository_id).await.is_empty());
    assert_eq!(runs(&pool).await, [("failed".to_string(), 0)]);
}
