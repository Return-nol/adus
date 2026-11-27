// Not every harness helper is used here.
#[allow(dead_code)]
mod common;

use std::sync::Arc;

use adus_core::job::Trigger;
use adus_core::types::Severity;
use adus_db::review_threads::{self, NewReviewThread, ReviewThread, ThreadOutcome, ThreadUpdate};
use adus_db::reviews::{self, ReviewStatus};
use adus_db::settings;
use adus_db::users::{self, Role};
use adus_git::{FileDiff, FileStatus, PrThread, PrThreadComment, ThreadRef, ThreadStatus};
use adus_llm::ReasoningEffort;
use adus_llm::router::ChatError;
use common::{FakeChat, FakeGit, NoopCloner, Setup, pr};
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

/// Every Adus reply starts with the marker, then the label on its own line.
const LABEL: &str = "<!-- adudu -->\n🤖 **Adudu**\n\n";

fn thread_ref() -> ThreadRef {
    ThreadRef {
        thread_id: "t0".into(),
        root_comment_id: "c0".into(),
    }
}

fn comment(id: &str, by_adus: bool, body: &str) -> PrThreadComment {
    PrThreadComment {
        id: id.into(),
        author: if by_adus { "pat-owner" } else { "octocat" }.into(),
        by_adus,
        body: body.into(),
    }
}

/// Adus's finding `c0`, then the given replies.
fn thread(replies: Vec<PrThreadComment>) -> PrThread {
    let mut comments = vec![comment("c0", true, "Retry loop has no backoff.")];
    comments.extend(replies);
    PrThread {
        id: Some("t0".into()),
        resolved: false,
        path: Some("src/a.rs".into()),
        line: Some(2),
        status: None,
        url: None,
        comments,
    }
}

/// GitHub repo with a default fast model, PR head `h2`, and Adus thread `t0` (finding on
/// `src/a.rs:2`, posted at `h1`) where `octocat` replied `c1`.
async fn setup(pool: &PgPool) -> (Setup, Arc<FakeChat>) {
    let chat = Arc::new(FakeChat::default());
    let diff = vec![FileDiff {
        path: "src/a.rs".into(),
        old_path: None,
        status: FileStatus::Modified,
        patch: Some("@@ -1,2 +1,3 @@\n fn retry() {\n+    sleep(backoff);\n }\n".into()),
    }];
    let s = common::setup(
        pool,
        FakeGit::new(pr("h2"), diff),
        chat.clone(),
        Arc::new(NoopCloner),
    )
    .await;
    common::default_fast_provider(pool).await;
    let row = NewReviewThread {
        repository_id: s.repository_id,
        review_id: None,
        pr_number: 7,
        provider_thread_id: "t0",
        root_comment_id: "c0",
        path: "src/a.rs",
        line: Some(2),
        severity: Severity::Warning,
        title: "Retry loop has no backoff",
        body: "Retry loop has no backoff.",
        posted_head_sha: "h1",
    };
    review_threads::insert_many(pool, &[row]).await.unwrap();
    s.git.state().thread = Some(thread(vec![comment("c1", false, "Fixed now.")]));
    (s, chat)
}

async fn row(pool: &PgPool, repository_id: Uuid) -> ReviewThread {
    review_threads::find(pool, repository_id, 7, "t0")
        .await
        .unwrap()
        .unwrap()
}

fn answer(verdict: &str, reason: &str, resolution: &str, reply: &str) -> String {
    json!({"verdict": verdict, "reason": reason, "resolution": resolution, "reply": reply})
        .to_string()
}

async fn reply_to(s: &Setup, thread_id: &str, comment_id: &str) {
    s.engine
        .thread_reply(s.repository_id, 7, thread_id, comment_id)
        .await
        .unwrap();
}

/// The bodies Adus posted in threads.
fn replies(s: &Setup) -> Vec<String> {
    s.git
        .state()
        .posted_threads
        .iter()
        .map(|(_, body)| body.clone())
        .collect()
}

#[sqlx::test(migrations = "../../migrations")]
async fn fixed_reply_resolves_thread(pool: PgPool) {
    let (s, chat) = setup(&pool).await;
    chat.reply(&answer(
        "fixed",
        "",
        "Retries now sleep with backoff.",
        "Confirmed, the retry now backs off.",
    ));

    reply_to(&s, "t0", "c1").await;

    let requests = chat.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].0.model, "gpt-fast");
    assert_eq!(requests[0].0.effort, Some(ReasoningEffort::Low));
    let prompt = chat.prompt();
    assert!(prompt.contains("<finding>"));
    assert!(prompt.contains("Fixed now."));
    assert!(prompt.contains("    2 +     sleep(backoff);"), "{prompt}");
    assert_eq!(
        s.git.state().statuses,
        [(thread_ref(), ThreadStatus::Fixed)]
    );
    let posted = replies(&s);
    assert_eq!(posted.len(), 1);
    assert_eq!(
        posted[0],
        format!("{LABEL}Confirmed, the retry now backs off.")
    );
    let row = row(&pool, s.repository_id).await;
    assert_eq!(row.outcome, ThreadOutcome::Fixed);
    assert_eq!(row.fix_sha.as_deref(), Some("h2"));
    assert_eq!(
        row.resolution.as_deref(),
        Some("Retries now sleep with backoff.")
    );
    assert_eq!(row.adus_replies, 1);
}

#[sqlx::test(migrations = "../../migrations")]
async fn by_design_with_reason_sets_status_and_reason(pool: PgPool) {
    let (s, chat) = setup(&pool).await;
    chat.reply(&answer(
        "by_design",
        "The PRD requires immediate retries.",
        "",
        "Understood: immediate retries are required.",
    ));

    reply_to(&s, "t0", "c1").await;

    assert_eq!(
        s.git.state().statuses,
        [(thread_ref(), ThreadStatus::ByDesign)]
    );
    assert!(replies(&s)[0].starts_with(LABEL));
    let row = row(&pool, s.repository_id).await;
    assert_eq!(row.outcome, ThreadOutcome::ByDesign);
    assert_eq!(
        row.reason.as_deref(),
        Some("The PRD requires immediate retries.")
    );
    assert_eq!(row.fix_sha, None);
}

#[sqlx::test(migrations = "../../migrations")]
async fn by_design_without_reason_asks_for_one(pool: PgPool) {
    let (s, chat) = setup(&pool).await;
    chat.reply(&answer("by_design", "  ", "", "Why is this intentional?"));

    reply_to(&s, "t0", "c1").await;

    assert!(s.git.state().statuses.is_empty());
    assert_eq!(replies(&s), [format!("{LABEL}Why is this intentional?")]);
    let row = row(&pool, s.repository_id).await;
    assert_eq!(row.outcome, ThreadOutcome::Open);
    assert_eq!(row.reason, None);
    assert_eq!(row.adus_replies, 1);
}

#[sqlx::test(migrations = "../../migrations")]
async fn not_fixed_on_fixed_thread_reopens(pool: PgPool) {
    let (s, chat) = setup(&pool).await;
    let fixed = ThreadUpdate {
        outcome: ThreadOutcome::Fixed,
        reason: None,
        fix_sha: Some("h1"),
        resolution: Some("Backoff added."),
    };
    let id = row(&pool, s.repository_id).await.id;
    review_threads::set_outcome(&pool, id, fixed).await.unwrap();
    chat.reply(&answer(
        "not_fixed",
        "",
        "The second retry path still has no backoff.",
        "Not quite: the second retry path still loops without backoff.",
    ));

    reply_to(&s, "t0", "c1").await;

    assert_eq!(
        s.git.state().statuses,
        [(thread_ref(), ThreadStatus::Active)]
    );
    assert!(replies(&s)[0].starts_with(LABEL));
    let row = row(&pool, s.repository_id).await;
    assert_eq!(row.outcome, ThreadOutcome::Open);
    assert_eq!(row.fix_sha, None);
}

#[sqlx::test(migrations = "../../migrations")]
async fn human_resolved_thread_is_synced_before_answering(pool: PgPool) {
    let (s, chat) = setup(&pool).await;
    {
        let mut state = s.git.state();
        let thread = state.thread.as_mut().unwrap();
        thread.status = Some("wontFix".into());
        thread.resolved = true;
    }
    chat.reply(&answer("discussion", "", "", "Noted."));

    reply_to(&s, "t0", "c1").await;

    assert!(s.git.state().statuses.is_empty());
    assert_eq!(replies(&s).len(), 1);
    let synced = row(&pool, s.repository_id).await;
    assert_eq!(synced.outcome, ThreadOutcome::WontFix);
    assert_eq!(synced.reason, None);

    // Adus answered a question, then the developer resolved the GitHub thread and said thanks.
    let reopen = || ThreadUpdate {
        outcome: ThreadOutcome::Open,
        reason: None,
        fix_sha: None,
        resolution: None,
    };
    review_threads::set_outcome(&pool, synced.id, reopen())
        .await
        .unwrap();
    let mut resolved = thread(vec![
        comment("c1", false, "Why?"),
        comment("c2", true, "Because retries can storm."),
        comment("c3", false, "Thanks, fixed."),
    ]);
    resolved.resolved = true;
    s.git.state().thread = Some(resolved);
    chat.reply(&answer("discussion", "", "", "Glad it helped."));

    reply_to(&s, "t0", "c3").await;

    assert_eq!(replies(&s).len(), 2);
    assert_eq!(
        row(&pool, s.repository_id).await.outcome,
        ThreadOutcome::Fixed
    );

    // The sync needs no LLM: it still runs when Adus may not reply.
    review_threads::set_outcome(&pool, synced.id, reopen())
        .await
        .unwrap();
    let mut current = settings::get(&pool).await.unwrap();
    current.max_thread_replies = 0;
    settings::update(&pool, &current).await.unwrap();

    reply_to(&s, "t0", "c3").await;

    assert_eq!(chat.requests().len(), 2);
    assert_eq!(
        row(&pool, s.repository_id).await.outcome,
        ThreadOutcome::Fixed
    );
    assert!(s.git.state().statuses.is_empty());
}

#[sqlx::test(migrations = "../../migrations")]
async fn reply_cap_or_zero_setting_skips_llm(pool: PgPool) {
    let (s, chat) = setup(&pool).await;
    let id = row(&pool, s.repository_id).await.id;

    // 0 = Adus never replies.
    let mut current = settings::get(&pool).await.unwrap();
    current.max_thread_replies = 0;
    settings::update(&pool, &current).await.unwrap();
    reply_to(&s, "t0", "c1").await;

    // The cap is reached.
    current.max_thread_replies = 2;
    settings::update(&pool, &current).await.unwrap();
    review_threads::record_reply(&pool, id).await.unwrap();
    review_threads::record_reply(&pool, id).await.unwrap();
    reply_to(&s, "t0", "c1").await;

    assert!(chat.requests().is_empty());
    assert!(replies(&s).is_empty());
}

#[sqlx::test(migrations = "../../migrations")]
async fn unknown_thread_is_ignored(pool: PgPool) {
    let (s, chat) = setup(&pool).await;

    reply_to(&s, "t9", "c1").await;

    assert!(chat.requests().is_empty());
    assert!(replies(&s).is_empty());
    assert!(s.git.state().comments.is_empty());
}

#[sqlx::test(migrations = "../../migrations")]
async fn unknown_command_outside_adus_thread_gets_help(pool: PgPool) {
    let (s, chat) = setup(&pool).await;
    s.git.state().thread = Some(thread(vec![comment("c1", false, "@adudu xyz")]));

    // `octocat` is not an Adus user yet: silent.
    reply_to(&s, "t9", "c1").await;
    assert!(s.git.state().comments.is_empty());

    users::insert(&pool, "octo@example.test", "x", Role::User, Some("octocat"))
        .await
        .unwrap();
    reply_to(&s, "t9", "c1").await;

    let comments = s.git.state().comments.clone();
    assert_eq!(comments.len(), 1);
    assert!(comments[0].starts_with(LABEL));
    assert!(comments[0].contains("@adudu review"));
    assert!(chat.requests().is_empty());
    assert!(replies(&s).is_empty());
}

#[sqlx::test(migrations = "../../migrations")]
async fn already_answered_comment_is_ignored(pool: PgPool) {
    let (s, chat) = setup(&pool).await;
    s.git.state().thread = Some(thread(vec![
        comment("c1", false, "Fixed now."),
        comment("c2", true, "Confirmed."),
    ]));

    reply_to(&s, "t0", "c1").await;

    assert!(chat.requests().is_empty());
    assert!(replies(&s).is_empty());
}

#[sqlx::test(migrations = "../../migrations")]
async fn comment_with_secret_is_redacted_in_prompt(pool: PgPool) {
    let (s, chat) = setup(&pool).await;
    let token = format!("ghp_{}", "a".repeat(36));
    s.git.state().thread = Some(thread(vec![comment(
        "c1",
        false,
        &format!("Use {token} to test it."),
    )]));
    chat.reply(&answer(
        "discussion",
        "",
        "",
        &format!("Never paste {token} here."),
    ));

    reply_to(&s, "t0", "c1").await;

    let prompt = chat.prompt();
    assert!(!prompt.contains(&token));
    assert!(prompt.contains("[REDACTED]"));
    let posted = replies(&s);
    assert_eq!(posted.len(), 1);
    assert!(!posted[0].contains(&token));
}

fn validate_answer(verdict: &str, resolution: &str, reply: &str) -> String {
    json!({
        "threads": [{
            "index": 0,
            "verdict": verdict,
            "resolution": resolution,
            "reply": reply,
        }]
    })
    .to_string()
}

#[sqlx::test(migrations = "../../migrations")]
async fn push_confirms_fixed_thread(pool: PgPool) {
    let (s, chat) = setup(&pool).await;
    s.git.state().incremental = vec![FileDiff {
        path: "src/a.rs".into(),
        old_path: None,
        status: FileStatus::Modified,
        patch: Some("@@ -1,2 +1,3 @@\n fn retry() {\n+    sleep(backoff);\n }\n".into()),
    }];
    chat.reply(&validate_answer(
        "fixed",
        "Added backoff sleep in retry loop.",
        "Confirmed fixed in the latest push.",
    ));
    chat.reply("[]");

    s.engine
        .review_pr(s.repository_id, 7, Trigger::Updated, None)
        .await
        .unwrap();

    let requests = chat.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].0.model, "gpt-fast");
    assert_eq!(requests[0].0.effort, Some(ReasoningEffort::Low));
    let row = row(&pool, s.repository_id).await;
    assert_eq!(row.outcome, ThreadOutcome::Fixed);
    assert_eq!(row.fix_sha.as_deref(), Some("h2"));
    assert_eq!(
        row.resolution.as_deref(),
        Some("Added backoff sleep in retry loop.")
    );
    assert_eq!(row.validated_head_sha.as_deref(), Some("h2"));
    assert_eq!(row.adus_replies, 1);
    assert_eq!(
        s.git.state().statuses,
        [(thread_ref(), ThreadStatus::Fixed)]
    );
    assert_eq!(
        replies(&s),
        [format!("{LABEL}Confirmed fixed in the latest push.")]
    );
    let reviews = s.git.state().reviews.clone();
    assert_eq!(reviews.len(), 1);
    assert!(
        reviews[0]
            .summary
            .contains("1 earlier finding(s) confirmed fixed."),
        "{}",
        reviews[0].summary
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn unchanged_head_makes_no_llm_call(pool: PgPool) {
    let (s, chat) = setup(&pool).await;
    s.git.state().pr.head_sha = "h1".into();
    let prev = reviews::insert(
        &pool,
        reviews::NewReview {
            repository_id: s.repository_id,
            pr_number: 7,
            trigger: Trigger::Opened,
        },
    )
    .await
    .unwrap();
    reviews::set_running(&pool, prev.id, "base", "h1")
        .await
        .unwrap();
    reviews::finish(&pool, prev.id, ReviewStatus::Done, 1, None, None)
        .await
        .unwrap();

    s.engine
        .review_pr(s.repository_id, 7, Trigger::Updated, None)
        .await
        .unwrap();

    assert!(chat.requests().is_empty());
    assert!(replies(&s).is_empty());
    assert!(s.git.state().statuses.is_empty());
    assert_eq!(row(&pool, s.repository_id).await.validated_head_sha, None);
}

#[sqlx::test(migrations = "../../migrations")]
async fn not_fixed_on_push_does_not_reply(pool: PgPool) {
    let (s, chat) = setup(&pool).await;
    s.git.state().incremental = vec![FileDiff {
        path: "src/a.rs".into(),
        old_path: None,
        status: FileStatus::Modified,
        patch: Some("@@ -1,2 +1,2 @@\n fn retry() {\n+    // unrelated comment\n }\n".into()),
    }];
    chat.reply(&validate_answer("not_fixed", "", "Still missing backoff."));
    chat.reply("[]");

    s.engine
        .review_pr(s.repository_id, 7, Trigger::Updated, None)
        .await
        .unwrap();

    let row = row(&pool, s.repository_id).await;
    assert_eq!(row.outcome, ThreadOutcome::Open);
    assert_eq!(row.fix_sha, None);
    assert_eq!(row.validated_head_sha.as_deref(), Some("h2"));
    assert_eq!(row.adus_replies, 0);
    assert!(s.git.state().statuses.is_empty());
    assert!(replies(&s).is_empty());
    assert!(
        !s.git.state().reviews[0]
            .summary
            .contains("earlier finding(s) confirmed fixed.")
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn validation_runs_when_review_is_skipped_as_draft(pool: PgPool) {
    let (s, chat) = setup(&pool).await;
    {
        let mut state = s.git.state();
        state.pr.is_draft = true;
        state.incremental = vec![FileDiff {
            path: "src/a.rs".into(),
            old_path: None,
            status: FileStatus::Modified,
            patch: Some("@@ -1,2 +1,3 @@\n fn retry() {\n+    sleep(backoff);\n }\n".into()),
        }];
    }
    chat.reply(&validate_answer(
        "fixed",
        "Backoff added.",
        "Confirmed fixed.",
    ));

    s.engine
        .review_pr(s.repository_id, 7, Trigger::Updated, None)
        .await
        .unwrap();

    assert_eq!(chat.requests().len(), 1);
    assert_eq!(chat.requests()[0].0.model, "gpt-fast");
    let row = row(&pool, s.repository_id).await;
    assert_eq!(row.outcome, ThreadOutcome::Fixed);
    assert_eq!(row.fix_sha.as_deref(), Some("h2"));
    assert_eq!(row.validated_head_sha.as_deref(), Some("h2"));
    assert_eq!(replies(&s), [format!("{LABEL}Confirmed fixed.")]);
    assert!(s.git.state().reviews.is_empty());
}

#[sqlx::test(migrations = "../../migrations")]
async fn deleted_file_fix_is_confirmed(pool: PgPool) {
    let (s, chat) = setup(&pool).await;
    let deleted = FileDiff {
        path: "src/a.rs".into(),
        old_path: None,
        status: FileStatus::Deleted,
        patch: Some("@@ -1,2 +0,0 @@\n-fn retry() {\n-}\n".into()),
    };
    {
        let mut state = s.git.state();
        state.full = vec![deleted.clone()];
        state.incremental = vec![deleted];
    }
    chat.reply(&validate_answer(
        "fixed",
        "File was deleted.",
        "Confirmed fixed: the file was removed.",
    ));

    s.engine
        .review_pr(s.repository_id, 7, Trigger::Updated, None)
        .await
        .unwrap();

    let prompt = chat.prompt();
    assert!(prompt.contains("\"deleted\""), "{prompt}");
    let row = row(&pool, s.repository_id).await;
    assert_eq!(row.outcome, ThreadOutcome::Fixed);
    assert_eq!(row.fix_sha.as_deref(), Some("h2"));
    assert_eq!(
        replies(&s),
        [format!("{LABEL}Confirmed fixed: the file was removed.")]
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn validation_error_does_not_fail_review(pool: PgPool) {
    let (s, chat) = setup(&pool).await;
    s.git.state().incremental = vec![FileDiff {
        path: "src/a.rs".into(),
        old_path: None,
        status: FileStatus::Modified,
        patch: Some("@@ -1,2 +1,3 @@\n fn retry() {\n+    sleep(backoff);\n }\n".into()),
    }];
    chat.reply_error(ChatError::AllKeysFailed { retry_after: None });
    chat.reply("[]");

    let review_id = s
        .engine
        .review_pr(s.repository_id, 7, Trigger::Updated, None)
        .await
        .unwrap()
        .unwrap();

    let review = reviews::last_done(&pool, s.repository_id, 7)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(review.id, review_id);
    assert_eq!(review.status, ReviewStatus::Done);
    assert_eq!(s.git.state().reviews.len(), 1);
    let row = row(&pool, s.repository_id).await;
    assert_eq!(row.outcome, ThreadOutcome::Open);
    assert_eq!(row.validated_head_sha, None);
}

#[sqlx::test(migrations = "../../migrations")]
async fn human_resolved_thread_is_synced_not_validated(pool: PgPool) {
    let (s, chat) = setup(&pool).await;
    {
        let mut state = s.git.state();
        state.incremental = vec![FileDiff {
            path: "src/a.rs".into(),
            old_path: None,
            status: FileStatus::Modified,
            patch: Some("@@ -1,2 +1,3 @@\n fn retry() {\n+    sleep(backoff);\n }\n".into()),
        }];
        state.thread.as_mut().unwrap().resolved = true;
    }
    chat.reply("[]");

    s.engine
        .review_pr(s.repository_id, 7, Trigger::Updated, None)
        .await
        .unwrap();

    let requests = chat.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].0.model, "gpt-test");
    let row = row(&pool, s.repository_id).await;
    assert_eq!(row.outcome, ThreadOutcome::Fixed);
    assert_eq!(row.fix_sha, None);
    assert_eq!(row.resolution, None);
    assert_eq!(row.validated_head_sha, None);
    assert!(s.git.state().statuses.is_empty());
    assert!(replies(&s).is_empty());
}
