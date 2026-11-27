// Not every harness helper is used here.
#[allow(dead_code)]
mod common;

use std::sync::Arc;

use adus_core::types::{LearningMode, Severity};
use adus_db::memories::{
    self, Evidence, Memory, MemoryKind, MemorySource, MemoryStatus, NewMemory,
};
use adus_db::repositories::{self, RepoPatch};
use adus_db::review_threads::{self, NewReviewThread, ThreadOutcome, ThreadUpdate};
use adus_db::rules::KnowledgeScope;
use adus_db::settings;
use adus_db::users::{self, Role, UserPatch};
use adus_git::{PrThread, PrThreadComment};
use adus_llm::ReasoningEffort;
use common::{FakeChat, FakeGit, NoopCloner, Setup, pr};
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

/// Engine over a GitHub repo in `mode`, with a provider whose primary model is `gpt-test`
/// and fast model is `gpt-fast`.
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
    common::default_fast_provider(pool).await;
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
        id: String::new(),
        author: author.into(),
        by_adus,
        body: body.into(),
    }
}

/// Adus flags something and `defender` answers that it is intentional.
fn rebutted(defender: &str, url: &str) -> PrThread {
    PrThread {
        id: None,
        resolved: false,
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

fn answer(memories: serde_json::Value) -> String {
    json!({ "memories": memories }).to_string()
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
        "kind": "exception",
        "content": "Retries without backoff are intentional.",
        "context": "By design: developer confirmed immediate retries.",
        "reason": "PRD requires immediate retries.",
        "resolution": null,
        "sources": [{"thread": 0, "comments": [1]}],
        "updates": null,
    }])));

    s.engine.learn_pr(s.repository_id, 7).await.unwrap();

    let memories = learned(&pool, s.repository_id).await;
    assert_eq!(memories.len(), 1);
    let memory = &memories[0];
    assert_eq!(memory.status, MemoryStatus::Pending);
    assert_eq!(memory.kind, MemoryKind::Exception);
    assert_eq!(memory.scope, KnowledgeScope::Repo);
    assert_eq!(memory.repository_id, Some(s.repository_id));
    assert_eq!(memory.content, "Retries without backoff are intentional.");
    assert_eq!(
        memory.context.as_deref(),
        Some("By design: developer confirmed immediate retries.")
    );
    assert_eq!(
        memory.reason.as_deref(),
        Some("PRD requires immediate retries.")
    );
    assert_eq!(memory.resolution, None);
    assert_eq!(memory.source_url.as_deref(), Some(url));
    assert_eq!(
        memory.evidence,
        vec![Evidence {
            url: url.to_string(),
            pr_number: Some(7),
        }]
    );
    assert_eq!(memory.created_by, None);
    assert_eq!(runs(&pool).await, [("learned".to_string(), 1)]);

    let requests = chat.requests();
    let (req, ctx) = &requests[0];
    assert_eq!(req.model, "gpt-test");
    assert_eq!(req.effort, Some(ReasoningEffort::Medium));
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
    // Threads are ordered newest-first in `<threads>`:
    // index 0 = joined (example.test/3), index 1 = stranger (example.test/2), index 2 = Octocat (example.test/1)
    chat.reply(&answer(json!([
        {"kind": "convention", "content": "A", "sources": [{"thread": 2, "comments": [1]}]},
        {"kind": "convention", "content": "B", "sources": [{"thread": 1, "comments": [1]}]},
        {"kind": "convention", "content": "C", "sources": [{"thread": 0, "comments": [2]}]},
        {"kind": "convention", "content": "invalid thread", "sources": [{"thread": 9, "comments": [1]}]},
        {"kind": "convention", "content": "invalid comment", "sources": [{"thread": 2, "comments": [5]}]},
        {"kind": "convention", "content": "  ", "sources": [{"thread": 2, "comments": [1]}]},
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
    chat.reply(&answer(json!([
        {"kind": "convention", "content": "A", "sources": [{"thread": 0, "comments": [1]}]}
    ])));

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
    chat.reply(&answer(json!([
        {"kind": "convention", "content": "A", "sources": [{"thread": 0, "comments": [1]}]}
    ])));

    s.engine.learn_pr(s.repository_id, 7).await.unwrap();
    s.engine.learn_pr(s.repository_id, 7).await.unwrap();

    assert_eq!(chat.requests().len(), 1);
    assert_eq!(learned(&pool, s.repository_id).await.len(), 1);
    assert_eq!(runs(&pool).await.len(), 1);
}

#[sqlx::test(migrations = "../../migrations")]
async fn learn_pr_without_qualifying_threads_makes_no_llm_call(pool: PgPool) {
    let adus_only = PrThread {
        id: Some("t-open".into()),
        resolved: false,
        path: Some("src/a.rs".into()),
        line: Some(3),
        status: None,
        url: Some("https://example.test/1".into()),
        comments: vec![comment("adudu-bot", true, "Unused variable.")],
    };
    let (s, chat) = setup(&pool, LearningMode::Approval, vec![adus_only]).await;
    review_threads::insert_many(
        &pool,
        &[NewReviewThread {
            repository_id: s.repository_id,
            review_id: None,
            pr_number: 7,
            provider_thread_id: "t-open",
            root_comment_id: "t-open",
            path: "src/a.rs",
            line: Some(3),
            severity: Severity::Warning,
            title: "Unused variable",
            body: "Remove it",
            posted_head_sha: "h1",
        }],
    )
    .await
    .unwrap();

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
async fn single_human_comment_becomes_convention(pool: PgPool) {
    let url = "https://github.com/acme/api/pull/7#discussion_r9";
    let single = PrThread {
        id: Some("h-1".into()),
        resolved: false,
        path: Some("src/lib.rs".into()),
        line: Some(12),
        status: None,
        url: Some(url.into()),
        comments: vec![comment(
            "reviewer",
            false,
            "Use `ApiJson<T>`, not `axum::Json`.",
        )],
    };
    let (s, chat) = setup(&pool, LearningMode::Approval, vec![single]).await;
    chat.reply(&answer(json!([{
        "kind": "convention",
        "content": "Extract JSON payloads with ApiJson<T> rather than axum::Json.",
        "context": "Reviewer pointed out the project extractor convention.",
        "sources": [{"thread": 0, "comments": [0]}],
    }])));

    s.engine.learn_pr(s.repository_id, 7).await.unwrap();

    let memories = learned(&pool, s.repository_id).await;
    assert_eq!(memories.len(), 1);
    assert_eq!(memories[0].kind, MemoryKind::Convention);
    assert_eq!(memories[0].status, MemoryStatus::Pending);
    assert_eq!(
        memories[0].content,
        "Extract JSON payloads with ApiJson<T> rather than axum::Json."
    );
    assert_eq!(memories[0].source_url.as_deref(), Some(url));
    assert_eq!(runs(&pool).await, [("learned".to_string(), 1)]);
}

#[sqlx::test(migrations = "../../migrations")]
async fn fixed_adus_thread_becomes_fix_pattern_active_in_auto(pool: PgPool) {
    let url = "https://github.com/acme/api/pull/7#discussion_r20";
    let adus_thread = PrThread {
        id: Some("adus-t1".into()),
        resolved: true,
        path: Some("src/db.rs".into()),
        line: Some(44),
        status: Some("fixed".into()),
        url: Some(url.into()),
        comments: vec![comment("adudu-bot", true, "Missing `.await` on query.")],
    };
    let (s, chat) = setup(&pool, LearningMode::Auto, vec![adus_thread]).await;
    review_threads::insert_many(
        &pool,
        &[NewReviewThread {
            repository_id: s.repository_id,
            review_id: None,
            pr_number: 7,
            provider_thread_id: "adus-t1",
            root_comment_id: "adus-t1",
            path: "src/db.rs",
            line: Some(44),
            severity: Severity::Critical,
            title: "Unawaited query",
            body: "SQLx query future is not awaited.",
            posted_head_sha: "h0",
        }],
    )
    .await
    .unwrap();
    let row = review_threads::find(&pool, s.repository_id, 7, "adus-t1")
        .await
        .unwrap()
        .unwrap();
    review_threads::set_outcome(
        &pool,
        row.id,
        ThreadUpdate {
            outcome: ThreadOutcome::Fixed,
            reason: None,
            fix_sha: Some("h1"),
            resolution: Some("Added `.await` to the query execution."),
        },
    )
    .await
    .unwrap();

    chat.reply(&answer(json!([{
        "kind": "fix_pattern",
        "content": "Always await SQLx query futures before returning.",
        "context": "Adus flagged an unawaited query and confirmed the fix on push.",
        "resolution": "Add `.await` to the SQLx query call.",
        "sources": [{"thread": 0, "comments": []}],
    }])));

    s.engine.learn_pr(s.repository_id, 7).await.unwrap();

    let memories = learned(&pool, s.repository_id).await;
    assert_eq!(memories.len(), 1);
    assert_eq!(memories[0].kind, MemoryKind::FixPattern);
    assert_eq!(memories[0].status, MemoryStatus::Active);
    assert_eq!(memories[0].created_by, None);
    assert_eq!(
        memories[0].resolution.as_deref(),
        Some("Add `.await` to the SQLx query call.")
    );
    assert_eq!(memories[0].source_url.as_deref(), Some(url));
    assert_eq!(runs(&pool).await, [("learned".to_string(), 1)]);
}

#[sqlx::test(migrations = "../../migrations")]
async fn exception_without_reason_is_dropped(pool: PgPool) {
    let threads = vec![rebutted("octocat", "https://example.test/1")];
    let (s, chat) = setup(&pool, LearningMode::Approval, threads).await;
    chat.reply(&answer(json!([
        {
            "kind": "exception",
            "content": "No backoff in retry loop.",
            "context": "By design: developer disagreed.",
            "reason": "   ",
            "sources": [{"thread": 0, "comments": [1]}],
        },
        {
            "kind": "fix_pattern",
            "content": "Avoid unawaited futures.",
            "resolution": null,
            "sources": [{"thread": 0, "comments": [1]}],
        }
    ])));

    s.engine.learn_pr(s.repository_id, 7).await.unwrap();

    assert!(learned(&pool, s.repository_id).await.is_empty());
    assert_eq!(runs(&pool).await, [("nothing_to_learn".to_string(), 0)]);
}

#[sqlx::test(migrations = "../../migrations")]
async fn updates_supersedes_existing_memory(pool: PgPool) {
    let url_new = "https://example.test/new";
    let (s, chat) = setup(
        &pool,
        LearningMode::Approval,
        vec![rebutted("octocat", url_new)],
    )
    .await;
    let user = users::insert(&pool, "octo@example.test", "x", Role::User, Some("octocat"))
        .await
        .unwrap();

    // Existing active memory to be updated (in Approval -> pending with supersedes).
    let old = memories::insert(
        &pool,
        NewMemory {
            scope: KnowledgeScope::Repo,
            repository_id: Some(s.repository_id),
            content: "Original convention",
            source: MemorySource::PrLearning,
            source_url: Some("https://example.test/old"),
            created_by: None,
            status: MemoryStatus::Active,
            kind: MemoryKind::Convention,
            context: None,
            reason: None,
            resolution: None,
            evidence: &[Evidence {
                url: "https://example.test/old".into(),
                pr_number: Some(2),
            }],
            supersedes: None,
        },
    )
    .await
    .unwrap();

    // Already-superseded memory that must be rejected as an `updates` target.
    let superseded = memories::insert(
        &pool,
        NewMemory {
            scope: KnowledgeScope::Repo,
            repository_id: Some(s.repository_id),
            content: "Retired convention",
            source: MemorySource::PrLearning,
            source_url: None,
            created_by: None,
            status: MemoryStatus::Active,
            kind: MemoryKind::Convention,
            context: None,
            reason: None,
            resolution: None,
            evidence: &[],
            supersedes: None,
        },
    )
    .await
    .unwrap();
    let _replacement = memories::insert(
        &pool,
        NewMemory {
            scope: KnowledgeScope::Repo,
            repository_id: Some(s.repository_id),
            content: "Replacement convention",
            source: MemorySource::PrLearning,
            source_url: None,
            created_by: None,
            status: MemoryStatus::Active,
            kind: MemoryKind::Convention,
            context: None,
            reason: None,
            resolution: None,
            evidence: &[],
            supersedes: Some(superseded.id),
        },
    )
    .await
    .unwrap();

    chat.reply(&answer(json!([
        {
            "kind": "convention",
            "content": "Updated convention",
            "sources": [{"thread": 0, "comments": [1]}],
            "updates": old.id.to_string(),
        },
        {
            "kind": "convention",
            "content": "Duplicate claim on old",
            "sources": [{"thread": 0, "comments": [1]}],
            "updates": old.id.to_string(),
        },
        {
            "kind": "convention",
            "content": "Already superseded target ignored",
            "sources": [{"thread": 0, "comments": [1]}],
            "updates": superseded.id.to_string(),
        },
        {
            "kind": "convention",
            "content": "Malformed uuid target ignored",
            "sources": [{"thread": 0, "comments": [1]}],
            "updates": "not-a-uuid",
        },
        {
            "kind": "unknown_kind",
            "content": "Malformed item skipped",
            "sources": [{"thread": 0, "comments": [1]}],
        }
    ])));

    s.engine.learn_pr(s.repository_id, 7).await.unwrap();

    let all = learned(&pool, s.repository_id).await;
    let updated = all
        .iter()
        .find(|m| m.content == "Updated convention")
        .unwrap();
    assert_eq!(updated.status, MemoryStatus::Pending);
    assert_eq!(updated.supersedes, Some(old.id));
    assert_eq!(
        updated.evidence,
        vec![
            Evidence {
                url: url_new.into(),
                pr_number: Some(7),
            },
            Evidence {
                url: "https://example.test/old".into(),
                pr_number: Some(2),
            },
        ]
    );
    // Duplicate claim, already-superseded target, and invalid UUID keep the memory with supersedes: None.
    for content in [
        "Duplicate claim on old",
        "Already superseded target ignored",
        "Malformed uuid target ignored",
    ] {
        let m = all.iter().find(|m| m.content == content).unwrap();
        assert_eq!(m.supersedes, None, "{content}");
    }
    assert!(!all.iter().any(|m| m.content == "Malformed item skipped"));
    // Old memory is not superseded yet while replacement is pending.
    assert_eq!(
        memories::get(&pool, old.id)
            .await
            .unwrap()
            .unwrap()
            .superseded_by,
        None
    );

    // Now test Auto mode: old memory is superseded immediately.
    repositories::update(
        &pool,
        s.repository_id,
        RepoPatch {
            learning_mode: Some(LearningMode::Auto),
            ..RepoPatch::default()
        },
    )
    .await
    .unwrap();
    s.git.state().pr = pr("h2");
    chat.reply(&answer(json!([{
        "kind": "convention",
        "content": "Auto-updated convention",
        "sources": [{"thread": 0, "comments": [1]}],
        "updates": old.id.to_string(),
    }])));

    s.engine.learn_pr(s.repository_id, 7).await.unwrap();

    let all2 = learned(&pool, s.repository_id).await;
    let auto_updated = all2
        .iter()
        .find(|m| m.content == "Auto-updated convention")
        .unwrap();
    assert_eq!(auto_updated.status, MemoryStatus::Active);
    assert_eq!(auto_updated.created_by, Some(user.id));
    assert_eq!(auto_updated.supersedes, Some(old.id));
    assert_eq!(
        memories::get(&pool, old.id)
            .await
            .unwrap()
            .unwrap()
            .superseded_by,
        Some(auto_updated.id)
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn cap_follows_setting(pool: PgPool) {
    let threads = vec![rebutted("octocat", "https://example.test/1")];
    let (s, chat) = setup(&pool, LearningMode::Approval, threads).await;
    let mut current = settings::get(&pool).await.unwrap();
    current.max_learned_per_pr = 2;
    settings::update(&pool, &current).await.unwrap();

    chat.reply(&answer(json!([
        {"kind": "convention", "content": "One", "sources": [{"thread": 0, "comments": [1]}]},
        {"kind": "convention", "content": "Two", "sources": [{"thread": 0, "comments": [1]}]},
        {"kind": "convention", "content": "Three", "sources": [{"thread": 0, "comments": [1]}]},
    ])));

    s.engine.learn_pr(s.repository_id, 7).await.unwrap();

    let memories = learned(&pool, s.repository_id).await;
    let contents: Vec<&str> = memories.iter().map(|m| m.content.as_str()).collect();
    assert_eq!(contents, ["One", "Two"]);
    assert_eq!(runs(&pool).await, [("learned".to_string(), 2)]);
}

#[sqlx::test(migrations = "../../migrations")]
async fn rejected_memory_is_listed_as_do_not_propose(pool: PgPool) {
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
            kind: MemoryKind::Convention,
            context: None,
            reason: None,
            resolution: None,
            evidence: &[],
            supersedes: None,
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
    assert!(known_list.chars().count() < 20_000 + 200);
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

#[sqlx::test(migrations = "../../migrations")]
async fn human_resolved_fixed_without_fix_sha_does_not_auto_activate(pool: PgPool) {
    let adus_thread = PrThread {
        id: Some("adus-human".into()),
        resolved: true,
        path: Some("src/db.rs".into()),
        line: Some(10),
        status: Some("fixed".into()),
        url: Some("https://github.com/acme/api/pull/7#discussion_r30".into()),
        comments: vec![comment("adudu-bot", true, "Missing `.await`.")],
    };
    let (s, chat) = setup(&pool, LearningMode::Auto, vec![adus_thread]).await;
    review_threads::insert_many(
        &pool,
        &[NewReviewThread {
            repository_id: s.repository_id,
            review_id: None,
            pr_number: 7,
            provider_thread_id: "adus-human",
            root_comment_id: "adus-human",
            path: "src/db.rs",
            line: Some(10),
            severity: Severity::Critical,
            title: "Unawaited query",
            body: "SQLx query future is not awaited.",
            posted_head_sha: "h0",
        }],
    )
    .await
    .unwrap();
    let row = review_threads::find(&pool, s.repository_id, 7, "adus-human")
        .await
        .unwrap()
        .unwrap();
    review_threads::set_outcome(
        &pool,
        row.id,
        ThreadUpdate {
            outcome: ThreadOutcome::Fixed,
            reason: None,
            fix_sha: None,
            resolution: None,
        },
    )
    .await
    .unwrap();

    chat.reply(&answer(json!([{
        "kind": "fix_pattern",
        "content": "Always await SQLx query futures.",
        "resolution": "Add `.await`.",
        "sources": [{"thread": 0, "comments": []}],
    }])));

    s.engine.learn_pr(s.repository_id, 7).await.unwrap();

    assert!(learned(&pool, s.repository_id).await.is_empty());
    assert_eq!(runs(&pool).await, [("nothing_to_learn".to_string(), 0)]);
}

#[sqlx::test(migrations = "../../migrations")]
async fn exception_context_prefix_enforced_and_curator_effort_forwarded(pool: PgPool) {
    let url = "https://github.com/acme/api/pull/7#discussion_r40";
    let (s, chat) = setup(
        &pool,
        LearningMode::Approval,
        vec![rebutted("octocat", url)],
    )
    .await;
    let mut current = settings::get(&pool).await.unwrap();
    current.curator_effort = "high".into();
    settings::update(&pool, &current).await.unwrap();

    chat.reply(&answer(json!([
        {
            "kind": "exception",
            "content": "Immediate retries are intentional.",
            "context": "Developer explained the retry requirement.",
            "reason": "PRD requires immediate retries.",
            "sources": [{"thread": 0, "comments": [1]}],
        },
        {
            "kind": "exception",
            "content": "Lowercase prefix is normalized.",
            "context": "by design: already has prefix.",
            "reason": "PRD requires immediate retries.",
            "sources": [{"thread": 0, "comments": [1]}],
        }
    ])));

    s.engine.learn_pr(s.repository_id, 7).await.unwrap();

    let requests = chat.requests();
    assert_eq!(requests[0].0.effort, Some(ReasoningEffort::High));
    let memories = learned(&pool, s.repository_id).await;
    assert_eq!(memories.len(), 2);
    assert_eq!(
        memories[0].context.as_deref(),
        Some("Won't fix: Developer explained the retry requirement.")
    );
    assert_eq!(
        memories[1].context.as_deref(),
        Some("By design: already has prefix.")
    );
}
