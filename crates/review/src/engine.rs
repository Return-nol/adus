//! Mesin review: ambil diff PR, jalankan Finder → Verifier → Dedup → Severity → Enrichment,
//! posting temuan, catat di `reviews`. With the clone, the Sharded Rules (`rules_judge.rs`) run
//! next to the Finder and their verified findings join the Verifier's before Dedup. The Finder
//! stage lives in `finder.rs`; this file keeps the review-row lifecycle, the clone and its
//! fallback, the stage sequencing and the deadlines, and posting.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use adus_core::crypto::Cipher;
use adus_core::job::Trigger;
use adus_core::types::{ConnectionKind, ReviewMode, Severity, normalize_path};
use adus_db::connections::{self, Connection, ConnectionStatus};
use adus_db::ignore::{IgnoreMatcher, default_ignore_paths, effective_ignore_paths, title_matches};
use adus_db::memories::Memory;
use adus_db::repositories::{self, Repository};
use adus_db::review_threads::{self, NewReviewThread};
use adus_db::reviews::{self, NewReview, ReviewStatus};
use adus_db::rules::Rule;
use adus_db::settings::Settings;
use adus_db::{ai, memories, rules, settings};
use adus_git::diff::{added_line_count, intersect_hunks, right_line_end_offset};
use adus_git::{
    FileDiff, FileStatus, GitError, GitProvider, GitProviderFactory, InlineComment, PostedComment,
    PrThread, PullRequest, RepoRef,
};
use adus_llm::ReasoningEffort;
use adus_llm::router::{ChatError, ChatService, UsageContext};
use sqlx::PgPool;
use tokio::time::Instant;
use uuid::Uuid;

use crate::agent::LlmContext;
use crate::budget::TokenBudget;
use crate::dedup::{DedupOutcome, deduplicate_findings};
use crate::discussion::threads_for_dedup;
use crate::enrich::enrich_suggestions;
use crate::finder::{
    FinderFailure, FinderInput, FinderLlm, FinderStats, SynthesisSkipped, collect_candidates,
    count_by_origin, finder_steps_and_refusals, run_finder,
};
use crate::parse::{CommentableRange, Suggestion};
use crate::prompt::{
    MAX_BATCH_CHARS, ManifestFile, REPLY_LABEL, SkipReason, adus_body, apply_precedent,
    batch_targets, build_manifest, format_inline, format_summary,
};
use crate::redact::{REDACTED, contains_secret, redact_secrets};
use crate::repo_instructions::load_repo_instructions;
use crate::rules_evidence::RuleEvidenceContext;
use crate::rules_judge::{ShardedRulesOutput, run_sharded_rules};
use crate::severity::{classify_severity, fill_unclassified};
use crate::small_pr::is_small_pr;
use crate::tools::Toolbox;
use crate::verifier::{CandidateFinding, VerificationReport, VerifyInput, verify_candidates};
use crate::workspace::{RepoCloner, WorkspaceError};

/// Budget of one Generalist review (`Engine::review_timeout`), small PRs included.
pub const REVIEW_TIMEOUT: Duration = Duration::from_secs(20 * 60);
/// Budget of one Specialists review (`Engine::specialists_review_timeout`): three agents per
/// batch at `High` effort need more time than one Generalist agent.
pub const SPECIALISTS_REVIEW_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// Hard cap on push validation before the review itself. Its time is not taken from the
/// review's budget (the budget starts after it), so the backstop in `review_pr` and the worker's
/// `JOB_TIMEOUT` both leave room for it.
pub const VALIDATE_TIMEOUT: Duration = Duration::from_secs(120);

/// Summary note when the clone failed (not for auth) and the review saw only the diff.
const CODEBASE_UNAVAILABLE: &str = "Codebase context unavailable — reviewed diff only";
/// Summary note for a small, low-risk PR reviewed without the clone (not a failure).
const SMALL_PR_NOTE: &str = "Small change — reviewed diff only";
/// Summary note when the model rejected tool calling.
const TOOLS_UNSUPPORTED: &str = "Model does not support tool calling — reviewed diff only";
/// Summary note when the fast model rejected the rule evidence collector's tool calling.
const RULES_TOOLS_UNSUPPORTED: &str =
    "Team rules not checked — the fast model does not support tool calling";
/// Last line of a comment whose suggestion was dropped because it held a secret.
const SUGGESTION_OMITTED: &str =
    "_Suggested code omitted: it contained a value that looks like a secret._";

pub struct Engine {
    pub db: PgPool,
    pub cipher: Arc<Cipher>,
    pub git: Arc<dyn GitProviderFactory>,
    pub chat: Arc<dyn ChatService>,
    /// Shallow-clones the PR head for the agentic tools (`GitCliCloner` in production).
    pub cloner: Arc<dyn RepoCloner>,
    /// Budget of a Generalist review (small PRs and `@adudu review generalist` included): a
    /// review still running when it is used up is stopped and recorded as failed (Fatal).
    /// Production: `REVIEW_TIMEOUT`; tests use short values. The stage deadlines are fractions
    /// of it.
    pub review_timeout: Duration,
    /// Budget of a Specialists review, used like `review_timeout`. Production:
    /// `SPECIALISTS_REVIEW_TIMEOUT`.
    pub specialists_review_timeout: Duration,
    /// Cap of push validation, which runs before the review budget starts. Production:
    /// `VALIDATE_TIMEOUT`.
    pub validate_timeout: Duration,
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum EngineError {
    /// Gangguan sementara (rate limit, jaringan, DB); job boleh diulang setelah `Duration` (jika ada).
    #[error("{0}")]
    Retryable(String, Option<Duration>),
    /// Mengulang tidak akan membantu.
    #[error("{0}")]
    Fatal(String),
}

/// Hasil review yang tidak gagal.
enum Outcome {
    /// On the PR. Its finding threads are recorded once the review deadlines are past.
    Done {
        head_sha: String,
        output: ReviewOutput,
        posted: Vec<PostedComment>,
    },
    /// Alasan snake_case, disimpan di `reviews.skip_reason`.
    Skipped(&'static str),
}

/// Satu file yang akan direview, beserta baris dan rentang hunk yang boleh dikomentari.
struct Target {
    file: FileDiff,
    allowed: CommentableRange,
}

#[derive(Default)]
struct ReviewOutput {
    counts: BTreeMap<Severity, usize>,
    comments: Vec<InlineComment>,
    /// The finding behind each of `comments`, same order.
    findings: Vec<Suggestion>,
    /// Tool results that had a secret masked.
    redacted_tool_outputs: u64,
    /// Inline comments that had a secret masked or their suggestion dropped.
    redacted_comments: usize,
}

/// Both Finder passes, in either review mode, run the primary model at this effort (spec §2).
const FINDER_EFFORT: ReasoningEffort = ReasoningEffort::High;

/// When each stage must be done, as fractions of the run's budget (`review_timeout` or
/// `specialists_review_timeout`) from the run's start (SPEC.md §8.4): Finder (clone included)
/// 55 %, Verifier 80 %, post-verify funnel 93 %; the rest is for posting.
#[derive(Debug, Clone, Copy)]
struct Deadlines {
    finder: Instant,
    verifier: Instant,
    post_verify: Instant,
}

impl Deadlines {
    fn new(start: Instant, budget: Duration) -> Self {
        let at = |percent: u32| start + budget.saturating_mul(percent) / 100;
        Self {
            finder: at(55),
            verifier: at(80),
            post_verify: at(93),
        }
    }
}

/// Everything the LLM stages of one review share.
struct Stages<'a> {
    conn: &'a Connection,
    git: &'a dyn GitProvider,
    repo_ref: &'a RepoRef,
    pr: &'a PullRequest,
    mode: ReviewMode,
    provider_id: Uuid,
    /// Finder and Verifier model.
    primary_model: &'a str,
    /// Fast model (`effective_fast_model()`): Finder Prose Recovery, the Sharded Rules and the
    /// post-verify funnel (Dedup, Severity and Enrichment).
    fast_model: &'a str,
    rules: &'a [Rule],
    memories: &'a [Memory],
    /// The PR's existing discussion (empty when it could not be fetched).
    threads: &'a [PrThread],
    usage: &'a UsageContext,
    deadlines: Deadlines,
    /// A small, low-risk PR: reviewed diff-only in one Generalist pass (`mode` is already
    /// `Generalist`).
    small: bool,
    /// Input tokens this run may send (`settings.max_review_input_tokens`), shared by every
    /// Finder, Sharded Rules, Verifier and post-verify call.
    token_budget: Arc<TokenBudget>,
}

impl<'a> Stages<'a> {
    fn llm<'b>(&'b self, chat: &'b dyn ChatService, model: &'b str) -> LlmContext<'b> {
        LlmContext {
            chat,
            provider_id: self.provider_id,
            model,
            usage: self.usage,
            token_budget: Some(&self.token_budget),
        }
    }

    fn primary_llm<'b>(&'b self, chat: &'b dyn ChatService) -> LlmContext<'b> {
        self.llm(chat, self.primary_model)
    }

    fn fast_llm<'b>(&'b self, chat: &'b dyn ChatService) -> LlmContext<'b> {
        self.llm(chat, self.fast_model)
    }
}

struct TargetInput<'a> {
    conn: &'a Connection,
    git: &'a dyn GitProvider,
    repo_ref: &'a RepoRef,
    repo_id: Uuid,
    pr_number: i64,
    review_id: Uuid,
    pr: &'a PullRequest,
    trigger: Trigger,
    settings: &'a Settings,
}

struct PreparedTargets {
    files: Vec<FileDiff>,
    allowed: HashMap<String, CommentableRange>,
    skipped: Vec<(String, SkipReason)>,
    ignored: usize,
    manifest: String,
}

enum TargetPrep {
    Ready(PreparedTargets),
    Skipped(&'static str),
}

impl Engine {
    /// Review satu PR. Return id baris `reviews`, atau `None` jika repo tidak ada / sudah dihapus
    /// (tidak ada repo untuk dicatat). Selesai, dilewati, dan `Fatal` meninggalkan baris; pada
    /// `Retryable` baris ini dihapus, karena percobaan ulang membuat baris baru (dan worker
    /// mencatat satu baris `failed` jika percobaannya habis).
    pub async fn review_pr(
        &self,
        repository_id: Uuid,
        pr_number: i64,
        trigger: Trigger,
        mode_override: Option<ReviewMode>,
    ) -> Result<Option<Uuid>, EngineError> {
        let Some(repo) = repositories::get(&self.db, repository_id)
            .await
            .map_err(db_error)?
        else {
            return Ok(None);
        };
        let new = NewReview {
            repository_id,
            pr_number,
            trigger,
        };
        let review_id = reviews::insert(&self.db, new).await.map_err(db_error)?.id;

        // Timeout di sini, bukan di worker, supaya baris review tetap dicatat gagal. Fatal:
        // mengulang PR yang sama hanya menghabiskan token LLM lagi sampai timeout lagi.
        // The clone lives inside `run`, so a timeout (or an aborted job) drops and deletes it.
        // `run` cuts itself at its own budget once the mode is known; this is the backstop. The
        // budget starts after push validation, so the backstop adds its cap.
        let backstop =
            self.review_timeout.max(self.specialists_review_timeout) + self.validate_timeout;
        let run = self.run(&repo, review_id, pr_number, trigger, mode_override);
        let outcome = match tokio::time::timeout(backstop, run).await {
            Ok(outcome) => outcome,
            Err(_elapsed) => Err(timed_out()),
        };
        let (status, posted, skip_reason) = match outcome {
            Ok(Outcome::Done {
                head_sha,
                output,
                posted,
            }) => {
                // After every review deadline (still under the job timeout): the review is on
                // the PR, so a slow insert must not turn it into a timed-out failure.
                self.record_threads(
                    repository_id,
                    review_id,
                    pr_number,
                    &head_sha,
                    &output,
                    &posted,
                )
                .await;
                (ReviewStatus::Done, posted.len(), None)
            }
            Ok(Outcome::Skipped(reason)) => (ReviewStatus::Skipped, 0, Some(reason)),
            Err(err) => {
                let recorded = match &err {
                    EngineError::Retryable(..) => reviews::delete(&self.db, review_id).await,
                    EngineError::Fatal(message) => {
                        let failed = ReviewStatus::Failed;
                        reviews::finish(&self.db, review_id, failed, 0, Some(message), None).await
                    }
                };
                // Error aslinya yang dikembalikan; gagal mencatat cukup di-log.
                if let Err(db_err) = recorded {
                    tracing::warn!(%review_id, error = %db_err, "could not record review failure");
                }
                return Err(err);
            }
        };
        let posted = i32::try_from(posted).unwrap_or(i32::MAX);
        // Review mungkin sudah terposting: jangan jadikan Retryable (akan posting dua kali).
        let finished = reviews::finish(&self.db, review_id, status, posted, None, skip_reason);
        if let Err(db_err) = finished.await {
            tracing::warn!(%review_id, error = %db_err, "could not record review result");
        }
        Ok(Some(review_id))
    }

    async fn run(
        &self,
        repo: &Repository,
        review_id: Uuid,
        pr_number: i64,
        trigger: Trigger,
        mode_override: Option<ReviewMode>,
    ) -> Result<Outcome, EngineError> {
        // Every budget counts from here, so the fetches and the clone count too.
        let start = Instant::now();
        if !repo.enabled {
            return Ok(Outcome::Skipped("repository_disabled"));
        }
        if trigger != Trigger::Command && !repo.auto_review {
            return Ok(Outcome::Skipped("auto_review_off"));
        }

        let conn = self.connection(repo).await?;
        let (git, repo_ref) = self.provider(&conn, repo)?;

        let pr = match git.get_pr(&repo_ref, pr_number).await {
            Ok(pr) => pr,
            Err(GitError::NotFound) => {
                return Err(EngineError::Fatal("pull request not found".into()));
            }
            Err(err) => return Err(self.fetch_error(&conn, err).await),
        };
        reviews::set_running(&self.db, review_id, &pr.base_sha, &pr.head_sha)
            .await
            .map_err(db_error)?;
        let settings = settings::get(&self.db).await.map_err(db_error)?;
        let is_draft_skip = pr.is_draft && trigger != Trigger::Command;
        let is_title_skip = trigger != Trigger::Command
            && title_matches(&pr.title, &settings.ignored_title_keywords);
        let models = if matches!(trigger, Trigger::Updated | Trigger::Command) {
            match self.models(repo, &settings).await {
                Ok(models) => models,
                Err(err) if is_draft_skip || is_title_skip => {
                    tracing::warn!(
                        %review_id,
                        error = %err,
                        "skipping push validation on skipped PR: could not load models"
                    );
                    None
                }
                Err(err) => return Err(err),
            }
        } else {
            None
        };
        let validation_started = Instant::now();
        let confirmed_fixed = if matches!(trigger, Trigger::Updated | Trigger::Command)
            && let Some((provider_id, _, fast_model)) = &models
        {
            match tokio::time::timeout(
                self.validate_timeout,
                self.validate_open_threads(
                    git.as_ref(),
                    &repo_ref,
                    repo,
                    &pr,
                    (*provider_id, fast_model.as_str()),
                    &settings,
                ),
            )
            .await
            {
                Ok(count) => count,
                Err(_) => {
                    tracing::warn!(%review_id, "push validation timed out");
                    0
                }
            }
        } else {
            0
        };
        let validation_elapsed = validation_started.elapsed();
        let start = start + validation_elapsed;
        if is_draft_skip {
            return Ok(Outcome::Skipped("draft"));
        }

        // Sebelum cek model dan diff: PR yang dilewati tidak memicu komentar atau fetch diff.
        if is_title_skip {
            return Ok(Outcome::Skipped("ignored_title"));
        }
        let models = match trigger {
            Trigger::Updated | Trigger::Command => models,
            Trigger::Opened => self.models(repo, &settings).await?,
        };
        let Some((provider_id, primary_model, fast_model)) = models else {
            // Azure mengirim `updated` juga untuk vote/reviewer: komentar hanya saat PR dibuka
            // atau diminta lewat `@adudu review`, supaya tidak spam.
            // Bukan kegagalan: tanpa baris failed atau dead letter untuk setiap push.
            if trigger == Trigger::Updated {
                return Ok(Outcome::Skipped("no_model"));
            }
            let body = adus_body(
                REPLY_LABEL,
                "Adudu can't review yet: no AI model is configured. \
                 An admin can pick a provider under Settings and its model under AI Providers.",
            );
            // Gagal posting tidak mengubah hasilnya: review tetap tidak bisa jalan.
            match git.post_comment(&repo_ref, pr_number, &body).await {
                Ok(()) => {}
                Err(GitError::Unauthorized) => {
                    self.unauthorized(&conn).await;
                }
                Err(err) => {
                    tracing::warn!(%review_id, error = %err, "could not post no-model comment")
                }
            }
            return Err(EngineError::Fatal("no review model configured".into()));
        };
        let mut mode = mode_override
            .or(repo.review_mode)
            .unwrap_or(settings.review_mode);

        // From here on the run is bounded by the budget it can still end with: a Generalist run
        // stays Generalist, a Specialists run may still be forced to Generalist (small PR). The
        // cut inside, at the effective mode's budget, stays the real one.
        let cut = start
            + match mode {
                ReviewMode::Generalist => self.review_timeout,
                ReviewMode::Specialists => self.specialists_review_timeout.max(self.review_timeout),
            };
        let rest = async {
            let mut targets = match self
                .prepare_targets(TargetInput {
                    conn: &conn,
                    git: git.as_ref(),
                    repo_ref: &repo_ref,
                    repo_id: repo.id,
                    pr_number,
                    review_id,
                    pr: &pr,
                    trigger,
                    settings: &settings,
                })
                .await?
            {
                TargetPrep::Ready(targets) => targets,
                TargetPrep::Skipped(reason) => return Ok(Outcome::Skipped(reason)),
            };

            let (small, rules, memories) = if targets.files.is_empty() {
                (false, Vec::new(), Vec::new())
            } else {
                let rules = rules::applicable(&self.db, repo.id)
                    .await
                    .map_err(db_error)?;
                let memories = memories::applicable(&self.db, repo.id)
                    .await
                    .map_err(db_error)?;
                // A small, low-risk PR is reviewed from the diff alone, in one Generalist pass: no
                // clone and no tools (the same diff-only path as a failed clone, but not a failure).
                // A `Command` (`@adudu review`) always gets the full review.
                let small_pr_max_lines = match trigger {
                    Trigger::Command => 0,
                    _ => u32::try_from(settings.small_pr_max_lines).unwrap_or(0),
                };
                let small = is_small_pr(&targets.files, &rules, small_pr_max_lines);
                if small {
                    mode = ReviewMode::Generalist;
                }
                (small, rules, memories)
            };
            // Read once, for Semantic Dedup. Without it the review still runs, as before.
            let threads = if targets.files.is_empty() {
                Vec::new()
            } else {
                git.list_pr_threads(&repo_ref, pr_number)
                    .await
                    .unwrap_or_else(|err| {
                        tracing::warn!(%review_id, error = %err, "could not list PR threads; deduplicating without them");
                        Vec::new()
                    })
            };

            let budget = self.budget(mode);
            let usage = UsageContext {
                review_id: Some(review_id),
                repository_id: Some(repo.id),
            };
            let stages = Stages {
                conn: &conn,
                git: git.as_ref(),
                repo_ref: &repo_ref,
                pr: &pr,
                mode,
                provider_id,
                primary_model: &primary_model,
                fast_model: &fast_model,
                rules: &rules,
                memories: &memories,
                threads: &threads,
                usage: &usage,
                deadlines: Deadlines::new(start, budget),
                small,
                token_budget: Arc::new(TokenBudget::new(
                    u64::try_from(settings.max_review_input_tokens).unwrap_or(0),
                )),
            };
            let mut warnings: Vec<String> = Vec::new();
            let finish = async {
                // Only skipped files left: no clone and no LLM, just the summary.
                let output = if targets.files.is_empty() {
                    ReviewOutput::default()
                } else {
                    self.review_targets(
                        &stages,
                        targets.files,
                        targets.allowed,
                        &targets.manifest,
                        &mut targets.skipped,
                        &mut warnings,
                    )
                    .await?
                };
                let summary = format_summary(
                    &output.counts,
                    &targets.skipped,
                    targets.ignored,
                    &warnings,
                    confirmed_fixed,
                );
                let (summary, summary_redacted) = redact_owned(summary);
                tracing::debug!(
                    redacted_tool_outputs = output.redacted_tool_outputs,
                    redacted_comments = output.redacted_comments + usize::from(summary_redacted),
                    "secret redaction"
                );
                match git
                    .post_review(&repo_ref, &pr, &output.comments, &summary)
                    .await
                {
                    Ok(posted) => Ok(Outcome::Done {
                        head_sha: pr.head_sha.clone(),
                        output,
                        posted,
                    }),
                    Err(err) => Err(self.post_error(&conn, err).await),
                }
            };
            match tokio::time::timeout_at(start + budget, finish).await {
                Ok(outcome) => outcome,
                Err(_elapsed) => Err(timed_out()),
            }
        };
        match tokio::time::timeout_at(cut, rest).await {
            Ok(outcome) => outcome,
            Err(_elapsed) => Err(timed_out()),
        }
    }

    /// Fetch incremental or full diff targets, filter ignored/empty/oversized/over-limit files,
    /// and build the PR manifest.
    async fn prepare_targets(&self, input: TargetInput<'_>) -> Result<TargetPrep, EngineError> {
        let TargetInput {
            conn,
            git,
            repo_ref,
            repo_id,
            pr_number,
            review_id,
            pr,
            trigger,
            settings,
        } = input;

        // Updated + review sebelumnya di commit lain → hanya baris dari commit baru.
        let last = match trigger {
            Trigger::Updated => reviews::last_done(&self.db, repo_id, pr_number)
                .await
                .map_err(db_error)?
                .and_then(|review| review.head_sha),
            _ => None,
        };
        if last.as_deref() == Some(pr.head_sha.as_str()) {
            return Ok(TargetPrep::Skipped("already_reviewed"));
        }
        let full = match git.pr_diff(repo_ref, pr).await {
            Ok(files) => files,
            Err(err) => return Err(self.fetch_error(conn, err).await),
        };
        // The PR file manifest lists every file of the PR, not only those reviewed now.
        let manifest_files: Vec<ManifestFile> = full.iter().map(ManifestFile::from_diff).collect();
        let candidates = match last {
            None => full_targets(full),
            Some(last_head) => match git.diff_between(repo_ref, &last_head, &pr.head_sha).await {
                Ok(changed) => incremental_targets(changed, full),
                // Setelah force-push commit lama bisa hilang; review penuh saja daripada gagal terus.
                Err(GitError::NotFound) => {
                    tracing::warn!(%review_id, "previous head not found, falling back to a full review");
                    full_targets(full)
                }
                Err(err) => return Err(self.fetch_error(conn, err).await),
            },
        };

        let matcher = ignore_matcher(&effective_ignore_paths(settings));
        let max_files = usize::try_from(settings.max_files_per_review).unwrap_or(0);
        let max_lines = usize::try_from(settings.max_patch_lines_per_file).unwrap_or(0);
        let mut allowed: HashMap<String, CommentableRange> = HashMap::new();
        let mut files: Vec<FileDiff> = Vec::new();
        let mut skipped: Vec<(String, SkipReason)> = Vec::new();
        let mut ignored_paths: Vec<String> = Vec::new();
        for target in candidates {
            // File terhapus tidak pernah direview, jadi juga tidak dihitung sebagai di-ignore.
            if target.file.status == FileStatus::Deleted {
                continue;
            }
            // Hanya `path` baru yang dicocokkan, juga untuk file yang di-rename.
            if let Some(matcher) = &matcher
                && matcher.is_ignored(&target.file.path)
            {
                ignored_paths.push(target.file.path);
                continue;
            }
            let Some(patch) = &target.file.patch else {
                skipped.push((target.file.path, SkipReason::NoPatch));
                continue;
            };
            if target.allowed.is_empty() {
                continue; // tidak ada baris baru yang bisa dikomentari
            }
            if added_line_count(patch) > max_lines {
                skipped.push((target.file.path, SkipReason::TooLarge));
            } else if files.len() >= max_files {
                skipped.push((target.file.path, SkipReason::FileLimit));
            } else {
                allowed.insert(normalize_path(&target.file.path), target.allowed);
                files.push(target.file);
            }
        }
        let ignored = ignored_paths.len();

        if files.is_empty() && skipped.is_empty() {
            return Ok(TargetPrep::Skipped("nothing_to_review"));
        }

        let manifest = if files.is_empty() {
            String::new()
        } else {
            let ignored_set: HashSet<&str> = ignored_paths.iter().map(String::as_str).collect();
            let skipped_map: HashMap<&str, SkipReason> = skipped
                .iter()
                .map(|(path, reason)| (path.as_str(), *reason))
                .collect();
            build_manifest(&manifest_files, &ignored_set, &skipped_map)
        };

        Ok(TargetPrep::Ready(PreparedTargets {
            files,
            allowed,
            skipped,
            ignored,
            manifest,
        }))
    }

    /// One `review_threads` row per posted finding the provider identified. The review is
    /// already posted: a DB error is only logged.
    async fn record_threads(
        &self,
        repository_id: Uuid,
        review_id: Uuid,
        pr_number: i64,
        head_sha: &str,
        output: &ReviewOutput,
        posted: &[PostedComment],
    ) {
        // The body is already redacted; the title is stored on its own.
        let titles: Vec<Cow<'_, str>> = output
            .findings
            .iter()
            .map(|f| redact_secrets(&f.title))
            .collect();
        let rows: Vec<NewReviewThread<'_>> = posted
            .iter()
            .filter(|p| !p.comment_id.is_empty())
            .filter_map(|p| {
                let comment = output.comments.get(p.index)?;
                let finding = output.findings.get(p.index)?;
                Some(NewReviewThread {
                    repository_id,
                    review_id: Some(review_id),
                    pr_number,
                    provider_thread_id: p.thread_id.as_deref().unwrap_or(&p.comment_id),
                    root_comment_id: &p.comment_id,
                    path: &comment.path,
                    line: i32::try_from(finding.line).ok(),
                    // `build_review_output` set every finding's severity.
                    severity: finding.severity?,
                    title: titles.get(p.index)?,
                    body: &comment.body,
                    posted_head_sha: head_sha,
                })
            })
            .collect();
        if let Err(err) = review_threads::insert_many(&self.db, &rows).await {
            tracing::warn!(%review_id, error = %err, "could not record finding threads");
        }
    }

    /// The time budget of a review in `mode` (the effective mode, after the small-PR force).
    fn budget(&self, mode: ReviewMode) -> Duration {
        match mode {
            ReviewMode::Generalist => self.review_timeout,
            ReviewMode::Specialists => self.specialists_review_timeout,
        }
    }

    /// Clone, Finder (with the Sharded Rules next to it when the clone has tools and a rule is
    /// enabled), Verifier and the post-verify funnel for `targets`. Returns the findings to
    /// post; files of a batch no Finder agent reviewed go to `skipped`, and every degradation
    /// adds a note to `warnings`.
    async fn review_targets(
        &self,
        s: &Stages<'_>,
        files: Vec<FileDiff>,
        allowed: HashMap<String, CommentableRange>,
        manifest: &str,
        skipped: &mut Vec<(String, SkipReason)>,
        warnings: &mut Vec<String>,
    ) -> Result<ReviewOutput, EngineError> {
        let mode = s.mode;

        // Clone: auth failure is fatal; anything else, or a clone still running at the Finder
        // deadline (spec decision 12: the clone is part of the Finder budget), falls back to
        // the diff only.
        let clone_started = Instant::now();
        let mut fallback_reason = None;
        let clone_spec = if s.small {
            warnings.push(SMALL_PR_NOTE.into());
            fallback_reason = Some("small_pr");
            None
        } else {
            s.git.clone_spec(s.repo_ref, s.pr)
        };
        let workspace = match clone_spec {
            None => None,
            Some(spec) => {
                let clone = self.cloner.clone_pr(&spec);
                match tokio::time::timeout_at(s.deadlines.finder, clone).await {
                    Ok(Ok(workspace)) => Some(Arc::new(workspace)),
                    Ok(Err(WorkspaceError::Unauthorized)) => {
                        return Err(self.unauthorized(s.conn).await);
                    }
                    Ok(Err(err)) => {
                        tracing::warn!(error = %err, "clone failed; reviewing the diff only");
                        warnings.push(CODEBASE_UNAVAILABLE.into());
                        fallback_reason = Some("clone_failed");
                        None
                    }
                    Err(_elapsed) => {
                        tracing::warn!("clone cut by the finder deadline; reviewing the diff only");
                        warnings.push(CODEBASE_UNAVAILABLE.into());
                        fallback_reason = Some("clone_timed_out");
                        None
                    }
                }
            }
        };
        let clone_ms = clone_started.elapsed().as_millis();
        let repo_instructions = match &workspace {
            Some(workspace) => load_repo_instructions(Arc::clone(workspace)).await,
            None => None,
        };
        let batches = batch_targets(&files, s.rules, MAX_BATCH_CHARS);
        // One cache for every agent of this review: the Finder, the Verifier and the rule
        // collectors (the shard calls read their head file past it).
        let toolbox = workspace.map(Toolbox::new);

        let input = FinderInput {
            pr: s.pr,
            manifest,
            batches: &batches,
            allowed: &allowed,
            memories: s.memories,
            repo_instructions: repo_instructions.as_deref(),
            mode,
        };
        let llm = FinderLlm {
            primary: s.primary_llm(self.chat.as_ref()),
            fast_model: s.fast_model,
            effort: FINDER_EFFORT,
        };
        // The Sharded Rules check the team rules only with the repository tools; diff-only,
        // the Finder prompt carries `<rules>` instead.
        let rules_ctx = toolbox
            .as_ref()
            .filter(|_| s.rules.iter().any(|rule| rule.enabled))
            .map(|toolbox| RuleEvidenceContext {
                llm: s.fast_llm(self.chat.as_ref()),
                toolbox,
            });
        let sharded_rules = async {
            match rules_ctx {
                Some(ctx) => {
                    run_sharded_rules(ctx, &files, &allowed, s.rules, s.deadlines.finder).await
                }
                None => ShardedRulesOutput::default(),
            }
        };
        // The Verifier starts as soon as the Finder is done and checks the Finder's candidates
        // only; the Sharded Rules run beside both by their own deadline, and their verified
        // findings join the Verifier's after both finish.
        let finder_and_verifier = async {
            let finder = run_finder(llm, &input, toolbox.as_ref(), s.deadlines.finder).await;
            // Read now: the summary names Finder wrap-ups only, not the Verifier's.
            let finder_wrap_ups = s.token_budget.wrap_ups();
            // From the final tasks, so a re-run without tools does not count a batch twice.
            let budget_skipped_batches = finder
                .tasks
                .iter()
                .filter(|task| matches!(task.result, Err(FinderFailure::TokenBudget)))
                .map(|task| task.batch)
                .collect::<HashSet<_>>()
                .len();
            // Without tool support the Verifier works from the diff as well.
            let verifier_toolbox = if finder.tools_unsupported {
                warnings.push(TOOLS_UNSUPPORTED.into());
                fallback_reason = Some("tools_unsupported");
                None
            } else {
                toolbox.as_ref()
            };
            let (finder_steps, gate_refusals) = finder_steps_and_refusals(&finder.tasks);
            let stats = FinderStats {
                tools_unsupported: finder.tools_unsupported,
                wrap_ups: finder_wrap_ups,
                budget_skipped_batches,
                tasks: finder.tasks.len(),
                steps: finder_steps,
                gate_refusals,
                synthesis_skipped: finder.synthesis_skipped.map(SynthesisSkipped::label),
                synthesis_candidates: finder.synthesis_candidates,
            };
            let (candidates, filtered_dropped) =
                collect_candidates(finder.tasks, mode, &batches, &allowed, skipped, warnings)?;
            // Every Finder candidate is queued for the Verifier (no count cap, spec §5).
            let queued = candidates.len();
            let mut report = if candidates.is_empty() {
                VerificationReport::default()
            } else {
                verify_candidates(
                    s.primary_llm(self.chat.as_ref()),
                    VerifyInput {
                        pr: s.pr,
                        manifest,
                        candidates,
                        targets: &files,
                        rules: s.rules,
                        memories: s.memories,
                        repo_instructions: repo_instructions.as_deref(),
                        toolbox: verifier_toolbox,
                        deadline: s.deadlines.verifier,
                    },
                )
                .await
            };
            if let Some(err) = report.all_retryable_error.take() {
                return Err(chat_error(err));
            }
            if report.dropped_unverified > 0 {
                warnings.push(format!(
                    "{} candidate finding(s) could not be verified and were omitted",
                    report.dropped_unverified
                ));
            }
            Ok((stats, filtered_dropped, queued, report))
        };
        let (finder_and_verifier, sharded_rules) = tokio::join!(finder_and_verifier, sharded_rules);
        let (finder, filtered_dropped, queued, mut report) = finder_and_verifier?;
        let ShardedRulesOutput {
            findings: rule_findings,
            shards_run: rule_shards,
            candidates: rule_candidates,
            verified: rule_verified,
            budget_skipped: rules_budget_skipped,
            tools_unsupported: rules_tools_unsupported,
        } = sharded_rules;
        // A Finder without tool support re-ran diff-only with `<rules>` in its prompt, so it
        // owns the team rules: rule findings would repeat its violations, and Dedup keeps both
        // copies when unsure. Otherwise rules a tool-less fast model could not check stay
        // unchecked.
        let (rule_findings, rules_budget_skipped) = if finder.tools_unsupported {
            if !rule_findings.is_empty() {
                tracing::debug!(
                    dropped = rule_findings.len(),
                    "rule findings dropped: the diff-only Finder re-run checked the team rules"
                );
            }
            (Vec::new(), 0)
        } else {
            if rules_tools_unsupported {
                warnings.push(RULES_TOOLS_UNSUPPORTED.into());
            }
            (rule_findings, rules_budget_skipped)
        };
        let mut verified = std::mem::take(&mut report.kept);
        let verified_kept = verified.len();
        verified.extend(rule_findings);
        let dedup_input = verified.len();

        let (found, kept_by_origin, already_raised) = run_post_verify_funnel(
            s,
            self.chat.as_ref(),
            toolbox.as_ref(),
            &files,
            &allowed,
            verified,
            warnings,
        )
        .await;

        let budget = s.token_budget.as_ref();
        // Only when the ceiling changed something (spec decision 15).
        if let Some(warning) = budget.warning(
            finder.budget_skipped_batches,
            finder.wrap_ups,
            rules_budget_skipped,
        ) {
            warnings.push(warning);
        }
        tracing::info!(
            review_id = s.usage.review_id.map(tracing::field::display),
            review_mode = ?mode,
            clone_ms,
            fallback_reason,
            finder_tasks = finder.tasks,
            finder_steps = finder.steps,
            gate_refusals = finder.gate_refusals,
            filtered_dropped,
            synthesis_candidates = finder.synthesis_candidates,
            synthesis_skipped = finder.synthesis_skipped,
            rule_shards,
            rule_candidates,
            rule_verified,
            verify_candidates = queued,
            verified_kept,
            verifier_kept_unreadable = report.kept_unreadable,
            precedents_cleared = report.precedents_cleared,
            dropped_refuted = report.dropped_refuted,
            dropped_unverified = report.dropped_unverified,
            evidence_gate_runs = report.evidence_gate_runs,
            dedup_dropped = dedup_input.saturating_sub(found.len() + already_raised),
            already_raised,
            posted = found.len(),
            kept_by_origin = %kept_by_origin,
            budget_used = budget.used(),
            input_tokens = budget.raw_input(),
            cache_read_tokens = budget.cache_read(),
            cache_write_tokens = budget.cache_write(),
            token_budget_reached = budget.exhausted(),
            "review stages finished"
        );
        Ok(build_review_output(&files, found, toolbox.as_ref()))
    }

    /// The repo's review provider (else the default one) when it is enabled and has a model:
    /// `(provider id, primary model, fast model)`, the fast model falling back to the primary.
    pub(crate) async fn models(
        &self,
        repo: &Repository,
        settings: &Settings,
    ) -> Result<Option<(Uuid, String, String)>, EngineError> {
        let provider = match repo.review_provider_id.or(settings.default_provider_id) {
            Some(id) => ai::get_provider(&self.db, id).await.map_err(db_error)?,
            None => None,
        }
        .filter(|p| p.enabled);
        Ok(provider.as_ref().and_then(|p| {
            let primary = p.primary_model()?;
            let fast = p.effective_fast_model().unwrap_or(primary);
            Some((p.id, primary.to_string(), fast.to_string()))
        }))
    }

    /// Connection milik repo.
    pub(crate) async fn connection(&self, repo: &Repository) -> Result<Connection, EngineError> {
        connections::get(&self.db, repo.connection_id)
            .await
            .map_err(db_error)?
            .ok_or_else(|| EngineError::Fatal("connection not found".into()))
    }

    /// Provider git dengan PAT connection yang sudah didekripsi, plus referensi repo-nya.
    pub(crate) fn provider(
        &self,
        conn: &Connection,
        repo: &Repository,
    ) -> Result<(Arc<dyn GitProvider>, RepoRef), EngineError> {
        let token = self
            .cipher
            .decrypt(&conn.encrypted_token)
            .map_err(|_| EngineError::Fatal("cannot decrypt the connection token".into()))?;
        let git = self.git.create(conn.kind, &conn.org_or_owner, &token);
        let repo_ref = RepoRef {
            external_id: repo.external_id.clone(),
            owner: repo.owner.clone(),
            project: repo.project.clone(),
            name: repo.name.clone(),
        };
        Ok((git, repo_ref))
    }

    /// Saat mengambil data, semua error selain token ditolak dianggap sementara.
    pub(crate) async fn fetch_error(&self, conn: &Connection, err: GitError) -> EngineError {
        match err {
            GitError::Unauthorized => self.unauthorized(conn).await,
            other => EngineError::Retryable(format!("git provider: {other}"), None),
        }
    }

    /// GitHub memposting review dalam satu request, jadi aman diulang saat rate limit, 5xx atau
    /// gangguan jaringan; 4xx lain (mis. 422) tidak akan berubah, jadi fatal. Azure memposting
    /// thread satu per satu: mengulang akan menggandakan thread yang sudah terposting, jadi
    /// selalu fatal.
    async fn post_error(&self, conn: &Connection, err: GitError) -> EngineError {
        let message = format!("posting the review failed: {err}");
        match (err, conn.kind) {
            (GitError::Unauthorized, _) => self.unauthorized(conn).await,
            (GitError::RateLimited | GitError::Http(_), ConnectionKind::Github) => {
                EngineError::Retryable(message, None)
            }
            (GitError::Status(status), ConnectionKind::Github) if status >= 500 => {
                EngineError::Retryable(message, None)
            }
            _ => EngineError::Fatal(message),
        }
    }

    /// PAT ditolak: tandai connection `invalid` supaya admin melihatnya di dashboard.
    pub(crate) async fn unauthorized(&self, conn: &Connection) -> EngineError {
        if let Err(err) =
            connections::set_status(&self.db, conn.id, ConnectionStatus::Invalid).await
        {
            return db_error(err);
        }
        EngineError::Fatal(format!(
            "the git provider rejected the token of connection \"{}\"",
            conn.name
        ))
    }
}

/// Post-verify funnel (spec §6), all by `deadlines.post_verify`: Semantic Dedup, severity,
/// and enrichment (which ends with `check_fix`).
async fn run_post_verify_funnel(
    s: &Stages<'_>,
    chat: &dyn ChatService,
    toolbox: Option<&Toolbox>,
    files: &[FileDiff],
    allowed: &HashMap<String, CommentableRange>,
    verified: Vec<CandidateFinding>,
    warnings: &mut Vec<String>,
) -> (Vec<Suggestion>, String, usize) {
    let finding_paths = verified
        .iter()
        .map(|c| normalize_path(&c.suggestion.file))
        .collect();
    let existing = threads_for_dedup(s.threads, &finding_paths);
    let fallback = verified.clone();
    let dedup = deduplicate_findings(s.fast_llm(chat), verified, &existing);
    let DedupOutcome {
        kept,
        already_raised,
    } = match tokio::time::timeout_at(s.deadlines.post_verify, dedup).await {
        Ok(outcome) => outcome,
        Err(_elapsed) => {
            tracing::warn!("semantic dedup cut by the deadline; keeping all findings");
            DedupOutcome {
                kept: fallback,
                already_raised: 0,
            }
        }
    };
    if already_raised > 0 {
        warnings.push(format!(
            "{already_raised} finding(s) skipped: already raised in this PR's discussion."
        ));
    }
    let kept_by_origin = count_by_origin(&kept);
    let mut found: Vec<Suggestion> = kept.into_iter().map(|c| c.suggestion).collect();

    let classify = classify_severity(s.fast_llm(chat), &mut found);
    if tokio::time::timeout_at(s.deadlines.post_verify, classify)
        .await
        .is_err()
    {
        tracing::warn!("severity classification cut by the deadline; using warning");
        fill_unclassified(&mut found);
    }
    // After the classifier, so a recorded exception's `Info` is the severity posted.
    for item in &mut found {
        apply_precedent(item, s.memories);
    }
    // Returns by the deadline on its own: no call starts in its last `ENRICH_RESERVE`.
    enrich_suggestions(
        s.fast_llm(chat),
        toolbox,
        files,
        allowed,
        &mut found,
        s.deadlines.post_verify,
    )
    .await;

    (found, kept_by_origin, already_raised)
}

fn build_review_output(
    files: &[FileDiff],
    mut found: Vec<Suggestion>,
    toolbox: Option<&Toolbox>,
) -> ReviewOutput {
    let patches: HashMap<String, Option<&str>> = files
        .iter()
        .map(|file| (normalize_path(&file.path), file.patch.as_deref()))
        .collect();
    let mut counts: BTreeMap<Severity, usize> = BTreeMap::new();
    let mut redacted_comments = 0;
    let comments = found
        .iter_mut()
        .map(|s| {
            // The one default left: `classify_severity`, or its deadline fallback, has
            // already given every finding a severity; the field stays an `Option` because
            // the Finder sends none. Set here so the recorded thread keeps the posted one.
            let severity = *s.severity.get_or_insert(Severity::Warning);
            *counts.entry(severity).or_default() += 1;
            let end_offset = s.start_line.and_then(|_| {
                let patch = patches.get(s.file.as_str()).copied().flatten()?;
                right_line_end_offset(patch, s.line)
            });
            let (body, redacted) = inline_body(s, severity);
            redacted_comments += usize::from(redacted);
            InlineComment {
                path: s.file.clone(),
                start_line: s.start_line,
                line: s.line,
                end_offset,
                body,
            }
        })
        .collect();
    ReviewOutput {
        counts,
        comments,
        findings: found,
        redacted_tool_outputs: toolbox.map_or(0, Toolbox::redacted_outputs),
        redacted_comments,
    }
}

/// The posted body of one finding with secrets masked. A suggestion that holds a secret, or a
/// value already masked as [`REDACTED`], is dropped (the comment keeps its span, and its body
/// ends with [`SUGGESTION_OMITTED`]), so "Apply suggestion" never writes `[REDACTED]` into
/// code. The flag: something was masked or dropped.
fn inline_body(s: &Suggestion, severity: Severity) -> (String, bool) {
    let leaks = s
        .suggestion
        .as_deref()
        .is_some_and(|code| contains_secret(code) || code.contains(REDACTED));
    let body = if leaks {
        format_inline(
            &Suggestion {
                suggestion: None,
                ..s.clone()
            },
            severity,
        )
    } else {
        format_inline(s, severity)
    };
    let (mut body, masked) = redact_owned(body);
    if leaks {
        body.push_str("\n\n");
        body.push_str(SUGGESTION_OMITTED);
    }
    (body, masked || leaks)
}

/// `text` with secrets masked, and whether anything was.
fn redact_owned(text: String) -> (String, bool) {
    if let Cow::Owned(masked) = redact_secrets(&text) {
        return (masked, true);
    }
    (text, false)
}

/// `AllKeysFailed`, `NoKeys` and `Internal` may pass when the job is retried later.
pub(crate) fn is_retryable(err: &ChatError) -> bool {
    matches!(
        err,
        ChatError::AllKeysFailed { .. } | ChatError::NoKeys | ChatError::Internal(_)
    )
}

/// Matcher daftar ignore. Daftar tersimpan yang tidak valid tidak boleh menggagalkan review:
/// pakai daftar bawaan, dan bila itu pun gagal, review tanpa ignore (`None`).
fn ignore_matcher(patterns: &[String]) -> Option<IgnoreMatcher> {
    match IgnoreMatcher::new(patterns) {
        Ok(matcher) => return Some(matcher),
        Err(err) => {
            tracing::warn!(error = %err, "stored ignore list is invalid, using the defaults")
        }
    }
    match IgnoreMatcher::new(&default_ignore_paths()) {
        Ok(matcher) => Some(matcher),
        Err(err) => {
            tracing::warn!(error = %err, "default ignore list is invalid, ignoring no files");
            None
        }
    }
}

/// Review penuh: semua baris `+` di patch PR boleh dikomentari.
fn full_targets(full: Vec<FileDiff>) -> Vec<Target> {
    full.into_iter()
        .map(|file| {
            let allowed = CommentableRange::from_patch(file.patch.as_deref().unwrap_or(""));
            Target { file, allowed }
        })
        .collect()
}

/// File dari commit baru yang masih ada di diff PR penuh. Baris yang boleh dikomentari harus
/// baru di commit ini DAN masih berubah di PR (supaya posisinya valid untuk provider).
fn incremental_targets(changed: Vec<FileDiff>, full: Vec<FileDiff>) -> Vec<Target> {
    let full_ranges: HashMap<String, CommentableRange> = full
        .into_iter()
        .map(|file| {
            let range = CommentableRange::from_patch(file.patch.as_deref().unwrap_or(""));
            (file.path, range)
        })
        .collect();
    changed
        .into_iter()
        .filter_map(|file| {
            let in_pr = full_ranges.get(&file.path)?;
            let new_range = CommentableRange::from_patch(file.patch.as_deref().unwrap_or(""));
            let allowed = CommentableRange {
                lines: new_range
                    .lines
                    .intersection(&in_pr.lines)
                    .copied()
                    .collect(),
                hunks: intersect_hunks(&new_range.hunks, &in_pr.hunks),
            };
            Some(Target { file, allowed })
        })
        .collect()
}

/// A review cut by its time budget. Fatal: a retry would spend the same LLM tokens again.
pub(crate) fn timed_out() -> EngineError {
    EngineError::Fatal("review timed out".into())
}

pub(crate) fn db_error(err: sqlx::Error) -> EngineError {
    EngineError::Retryable(format!("database: {err}"), None)
}

pub(crate) fn chat_error(err: ChatError) -> EngineError {
    match err {
        ChatError::AllKeysFailed { retry_after } => {
            EngineError::Retryable(format!("LLM: {err}"), retry_after)
        }
        ChatError::NoKeys | ChatError::Internal(_) => {
            EngineError::Retryable(format!("LLM: {err}"), None)
        }
        ChatError::Invalid(_) | ChatError::ToolsUnsupported(_) | ChatError::ContextOverflow(_) => {
            EngineError::Fatal(format!("LLM: {err}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deadlines_split_budget_55_80_93() {
        let start = Instant::now();
        let d = Deadlines::new(start, Duration::from_secs(100));
        assert_eq!(d.finder, start + Duration::from_secs(55));
        assert_eq!(d.verifier, start + Duration::from_secs(80));
        assert_eq!(d.post_verify, start + Duration::from_secs(93));
    }
}
