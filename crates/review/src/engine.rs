//! Mesin review: ambil diff PR, jalankan Finder → Verifier → Dedup, posting temuan, catat di
//! `reviews`. The Finder stage lives in `finder.rs`; this file keeps the review-row lifecycle,
//! the clone and its fallback, the stage sequencing and the deadlines, and posting.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use adus_core::crypto::Cipher;
use adus_core::job::Trigger;
use adus_core::types::{ConnectionKind, ReviewMode, Severity, normalize_path};
use adus_db::connections::{self, Connection, ConnectionStatus};
use adus_db::ignore::{IgnoreMatcher, default_ignore_paths, effective_ignore_paths, title_matches};
use adus_db::memories::Memory;
use adus_db::repositories::{self, Repository};
use adus_db::reviews::{self, NewReview, ReviewStatus};
use adus_db::rules::Rule;
use adus_db::{ai, memories, rules, settings};
use adus_git::diff::{added_line_count, commentable_lines};
use adus_git::{
    FileDiff, FileStatus, GitError, GitProvider, GitProviderFactory, InlineComment, PullRequest,
    RepoRef,
};
use adus_llm::ReasoningEffort;
use adus_llm::router::{ChatError, ChatService, UsageContext};
use sqlx::PgPool;
use tokio::time::Instant;
use uuid::Uuid;

use crate::agent::AgentError;
use crate::dedup::deduplicate_findings;
use crate::finder::{FinderFailure, FinderInput, FinderLlm, FinderTask, run_finder};
use crate::parse::{Suggestion, keep_valid_batch};
use crate::prompt::{
    DiffBatch, MARKER, MAX_BATCH_CHARS, batch_targets, build_manifest, format_inline,
    format_summary,
};
use crate::repo_instructions::load_repo_instructions;
use crate::tools::{ToolCache, Toolbox};
use crate::verifier::{CandidateFinding, VerifyContext, verify_candidates};
use crate::workspace::{RepoCloner, WorkspaceError};

/// Batas waktu bawaan satu review (`Engine::review_timeout`).
pub const REVIEW_TIMEOUT: Duration = Duration::from_secs(12 * 60);

/// Summary note when the clone failed (not for auth) and the review saw only the diff.
const CODEBASE_UNAVAILABLE: &str = "Codebase context unavailable — reviewed diff only";
/// Summary note when the model rejected tool calling.
const TOOLS_UNSUPPORTED: &str = "Model does not support tool calling — reviewed diff only";

pub struct Engine {
    pub db: PgPool,
    pub cipher: Arc<Cipher>,
    pub git: Arc<dyn GitProviderFactory>,
    pub chat: Arc<dyn ChatService>,
    /// Shallow-clones the PR head for the agentic tools (`GitCliCloner` in production).
    pub cloner: Arc<dyn RepoCloner>,
    /// Review yang lebih lama dari ini dihentikan dan dicatat gagal (Fatal). Produksi:
    /// `REVIEW_TIMEOUT`; test memakai nilai pendek. The stage deadlines are fractions of it.
    pub review_timeout: Duration,
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
    Done {
        posted: usize,
    },
    /// Alasan snake_case, disimpan di `reviews.skip_reason`.
    Skipped(&'static str),
}

/// Satu file yang akan direview, beserta baris yang boleh dikomentari.
struct Target {
    file: FileDiff,
    allowed: BTreeSet<u32>,
}

pub fn finder_effort(mode: ReviewMode) -> ReasoningEffort {
    match mode {
        ReviewMode::Generalist => ReasoningEffort::Medium,
        ReviewMode::Specialists => ReasoningEffort::High,
    }
}

/// When each stage must be done, as fractions of `review_timeout` from the run's start
/// (spec 4.11): Finder (clone included) 60 %, Verifier 90 %, Dedup 97 %; the rest is for posting.
#[derive(Debug, Clone, Copy)]
struct Deadlines {
    finder: Instant,
    verifier: Instant,
    dedup: Instant,
}

impl Deadlines {
    fn new(start: Instant, budget: Duration) -> Self {
        let at = |percent: u32| start + budget.saturating_mul(percent) / 100;
        Self {
            finder: at(60),
            verifier: at(90),
            dedup: at(97),
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
    /// Finder model.
    primary_model: &'a str,
    /// Verifier and Dedup model (`effective_fast_model()`).
    fast_model: &'a str,
    rules: &'a [Rule],
    memories: &'a [Memory],
    usage: &'a UsageContext,
    deadlines: Deadlines,
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
        let run = self.run(&repo, review_id, pr_number, trigger, mode_override);
        let outcome = match tokio::time::timeout(self.review_timeout, run).await {
            Ok(outcome) => outcome,
            Err(_elapsed) => Err(EngineError::Fatal("review timed out".into())),
        };
        let (status, posted, skip_reason) = match outcome {
            Ok(Outcome::Done { posted }) => (ReviewStatus::Done, posted, None),
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
        let deadlines = Deadlines::new(Instant::now(), self.review_timeout);
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
        if pr.is_draft && trigger != Trigger::Command {
            return Ok(Outcome::Skipped("draft"));
        }

        let settings = settings::get(&self.db).await.map_err(db_error)?;
        // Sebelum cek model dan diff: PR yang dilewati tidak memicu komentar atau fetch diff.
        if trigger != Trigger::Command && title_matches(&pr.title, &settings.ignored_title_keywords)
        {
            return Ok(Outcome::Skipped("ignored_title"));
        }
        let provider_id = repo.review_provider_id.or(settings.default_provider_id);
        let provider = match provider_id {
            Some(id) => ai::get_provider(&self.db, id).await.map_err(db_error)?,
            None => None,
        }
        .filter(|p| p.enabled);
        let models = provider.as_ref().and_then(|p| {
            let primary = p.primary_model()?;
            let fast = p.effective_fast_model().unwrap_or(primary);
            Some((p.id, primary.to_string(), fast.to_string()))
        });
        let Some((provider_id, primary_model, fast_model)) = models else {
            // Azure mengirim `updated` juga untuk vote/reviewer: komentar hanya saat PR dibuka
            // atau diminta lewat `@adudu review`, supaya tidak spam.
            // Bukan kegagalan: tanpa baris failed atau dead letter untuk setiap push.
            if trigger == Trigger::Updated {
                return Ok(Outcome::Skipped("no_model"));
            }
            let body = format!(
                "{MARKER}\nAdudu can't review yet: no AI model is configured. \
                 An admin can pick a provider under Settings and its model under AI Providers."
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
        let mode = mode_override
            .or(repo.review_mode)
            .unwrap_or(settings.review_mode);

        // Updated + review sebelumnya di commit lain → hanya baris dari commit baru.
        let last = match trigger {
            Trigger::Updated => reviews::last_done(&self.db, repo.id, pr_number)
                .await
                .map_err(db_error)?
                .and_then(|review| review.head_sha),
            _ => None,
        };
        if last.as_deref() == Some(pr.head_sha.as_str()) {
            return Ok(Outcome::Skipped("already_reviewed"));
        }
        let full = match git.pr_diff(&repo_ref, &pr).await {
            Ok(files) => files,
            Err(err) => return Err(self.fetch_error(&conn, err).await),
        };
        // The PR file manifest lists every file of the PR, not only those reviewed now.
        let pr_files = full.clone();
        let candidates = match last {
            None => full_targets(full),
            Some(last_head) => match git.diff_between(&repo_ref, &last_head, &pr.head_sha).await {
                Ok(changed) => incremental_targets(changed, full),
                // Setelah force-push commit lama bisa hilang; review penuh saja daripada gagal terus.
                Err(GitError::NotFound) => {
                    tracing::warn!(%review_id, "previous head not found, falling back to a full review");
                    full_targets(full)
                }
                Err(err) => return Err(self.fetch_error(&conn, err).await),
            },
        };

        let matcher = ignore_matcher(&effective_ignore_paths(&settings));
        let max_files = usize::try_from(settings.max_files_per_review).unwrap_or(0);
        let max_lines = usize::try_from(settings.max_patch_lines_per_file).unwrap_or(0);
        let mut targets = Vec::new();
        let mut skipped: Vec<(String, &'static str)> = Vec::new();
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
                skipped.push((target.file.path, "no_patch"));
                continue;
            };
            if target.allowed.is_empty() {
                continue; // tidak ada baris baru yang bisa dikomentari
            }
            if added_line_count(patch) > max_lines {
                skipped.push((target.file.path, "too_large"));
            } else if targets.len() >= max_files {
                skipped.push((target.file.path, "file_limit"));
            } else {
                targets.push(target);
            }
        }
        let ignored = ignored_paths.len();

        if targets.is_empty() && skipped.is_empty() {
            return Ok(Outcome::Skipped("nothing_to_review"));
        }

        let mut warnings: Vec<String> = Vec::new();
        // Only skipped files left: no clone and no LLM, just the summary.
        let found = if targets.is_empty() {
            Vec::new()
        } else {
            let ignored_set: HashSet<&str> = ignored_paths.iter().map(String::as_str).collect();
            let skipped_map: HashMap<&str, &'static str> = skipped
                .iter()
                .map(|(path, reason)| (path.as_str(), *reason))
                .collect();
            let manifest = build_manifest(&pr_files, &ignored_set, &skipped_map);
            let rules = rules::applicable(&self.db, repo.id)
                .await
                .map_err(db_error)?;
            let memories = memories::applicable(&self.db, repo.id)
                .await
                .map_err(db_error)?;
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
                usage: &usage,
                deadlines,
            };
            self.review_targets(&stages, targets, &manifest, &mut skipped, &mut warnings)
                .await?
        };

        let mut counts: BTreeMap<Severity, usize> = BTreeMap::new();
        for suggestion in &found {
            *counts.entry(suggestion.severity).or_default() += 1;
        }
        let comments: Vec<InlineComment> = found
            .iter()
            .map(|s| InlineComment {
                path: s.file.clone(),
                line: s.line,
                body: format_inline(s),
            })
            .collect();
        let summary = format_summary(&counts, &skipped, ignored, &warnings);
        match git.post_review(&repo_ref, &pr, &comments, &summary).await {
            Ok(posted) => Ok(Outcome::Done { posted }),
            Err(err) => Err(self.post_error(&conn, err).await),
        }
    }

    /// Clone, Finder, Verifier and Dedup for `targets`. Returns the findings to post; files of
    /// a batch no Finder agent reviewed go to `skipped`, and every degradation adds a note to
    /// `warnings`.
    async fn review_targets(
        &self,
        s: &Stages<'_>,
        targets: Vec<Target>,
        manifest: &str,
        skipped: &mut Vec<(String, &'static str)>,
        warnings: &mut Vec<String>,
    ) -> Result<Vec<Suggestion>, EngineError> {
        let mut allowed: HashMap<String, BTreeSet<u32>> = HashMap::new();
        let files: Vec<FileDiff> = targets
            .into_iter()
            .map(|target| {
                allowed.insert(normalize_path(&target.file.path), target.allowed);
                target.file
            })
            .collect();
        let batches = batch_targets(&files, s.rules, MAX_BATCH_CHARS);

        // Clone: auth failure is fatal; anything else falls back to the diff only.
        let clone_started = Instant::now();
        let mut fallback_reason = None;
        let workspace = match s.git.clone_spec(s.repo_ref, s.pr) {
            None => None,
            Some(spec) => match self.cloner.clone_pr(&spec).await {
                Ok(workspace) => Some(Arc::new(workspace)),
                Err(WorkspaceError::Unauthorized) => return Err(self.unauthorized(s.conn).await),
                Err(err) => {
                    tracing::warn!(error = %err, "clone failed; reviewing the diff only");
                    warnings.push(CODEBASE_UNAVAILABLE.into());
                    fallback_reason = Some("clone_failed");
                    None
                }
            },
        };
        let clone_ms = clone_started.elapsed().as_millis();
        let repo_instructions = match &workspace {
            Some(workspace) => load_repo_instructions(Arc::clone(workspace)).await,
            None => None,
        };
        // One cache for every agent of this review, Finder and Verifier alike.
        let toolbox = workspace.map(|ws| Toolbox::new(ws, Arc::new(ToolCache::new())));

        let input = FinderInput {
            pr: s.pr,
            manifest,
            batches: &batches,
            memories: s.memories,
            repo_instructions: repo_instructions.as_deref(),
            mode: s.mode,
        };
        let llm = FinderLlm {
            chat: self.chat.as_ref(),
            provider_id: s.provider_id,
            model: s.primary_model,
            effort: finder_effort(s.mode),
            usage: s.usage,
        };
        let finder = run_finder(llm, &input, toolbox.as_ref(), s.deadlines.finder).await;
        // Without tool support the Verifier works from the diff as well.
        let verifier_toolbox = if finder.tools_unsupported {
            warnings.push(TOOLS_UNSUPPORTED.into());
            fallback_reason = Some("tools_unsupported");
            None
        } else {
            toolbox.as_ref()
        };
        let finder_tasks = finder.tasks.len();
        let (cache_read_tokens, cache_write_tokens) = finder_cache_tokens(&finder.tasks);
        let candidates = collect_candidates(finder.tasks, &batches, &allowed, skipped, warnings)?;
        let candidate_count = candidates.len();

        let (verified, verifier_dropped) = if candidates.is_empty() {
            (Vec::new(), 0)
        } else {
            let ctx = VerifyContext {
                chat: self.chat.as_ref(),
                provider_id: s.provider_id,
                model: s.fast_model,
                usage: s.usage,
            };
            let report = verify_candidates(
                ctx,
                candidates,
                &files,
                s.rules,
                s.memories,
                repo_instructions.as_deref(),
                verifier_toolbox,
                s.deadlines.verifier,
            )
            .await;
            if let Some(err) = report.all_retryable_error {
                return Err(chat_error(err));
            }
            if report.dropped_unverified > 0 {
                warnings.push(format!(
                    "{} candidate finding(s) could not be verified and were omitted",
                    report.dropped_unverified
                ));
            }
            if report.truncated_by_cap > 0 {
                warnings.push(format!(
                    "{} lower-priority findings exceeded the verification cap and were omitted",
                    report.truncated_by_cap
                ));
            }
            let dropped = report.dropped_refuted + report.dropped_unverified;
            (report.kept, dropped)
        };
        let verified_kept = verified.len();

        let found: Vec<Suggestion> = if verified.len() < 2 {
            verified.into_iter().map(|c| c.suggestion).collect()
        } else {
            let fallback: Vec<Suggestion> = verified.iter().map(|c| c.suggestion.clone()).collect();
            let dedup = deduplicate_findings(
                self.chat.as_ref(),
                s.provider_id,
                s.fast_model,
                s.usage,
                verified,
            );
            match tokio::time::timeout_at(s.deadlines.dedup, dedup).await {
                Ok(result) => result.kept,
                Err(_elapsed) => {
                    tracing::warn!("semantic dedup cut by the deadline; keeping all findings");
                    fallback
                }
            }
        };

        tracing::info!(
            review_mode = ?s.mode,
            clone_ms,
            fallback_reason,
            finder_tasks,
            candidates = candidate_count,
            verified_kept,
            verifier_dropped,
            dedup_dropped = verified_kept - found.len(),
            cache_read_tokens,
            cache_write_tokens,
            "review stages finished"
        );
        Ok(found)
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
    async fn fetch_error(&self, conn: &Connection, err: GitError) -> EngineError {
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

/// Cache tokens summed over the Finder agents that finished.
fn finder_cache_tokens(tasks: &[FinderTask]) -> (u32, u32) {
    tasks
        .iter()
        .filter_map(|task| task.result.as_ref().ok())
        .fold((0u32, 0u32), |(read, write), outcome| {
            (
                read.saturating_add(outcome.cache_read_tokens),
                write.saturating_add(outcome.cache_write_tokens),
            )
        })
}

/// Turns Finder results into candidates for the Verifier, in (batch, specialist) order: each
/// result is filtered to its batch's commentable lines, then exact `(file, line, title)`
/// duplicates are dropped across the whole review.
///
/// A failed task becomes a summary note when another agent reviewed its batch (or when the
/// deadline cut it); when no agent reviewed a batch, its files are listed as skipped. When no
/// task succeeded at all, the review fails (see [`all_failed`]).
fn collect_candidates(
    tasks: Vec<FinderTask>,
    batches: &[DiffBatch<'_>],
    allowed: &HashMap<String, BTreeSet<u32>>,
    skipped: &mut Vec<(String, &'static str)>,
    warnings: &mut Vec<String>,
) -> Result<Vec<CandidateFinding>, EngineError> {
    if !tasks.is_empty() && tasks.iter().all(|task| task.result.is_err()) {
        return Err(all_failed(tasks));
    }
    let reviewed: HashSet<usize> = tasks
        .iter()
        .filter(|task| task.result.is_ok())
        .map(|task| task.batch)
        .collect();
    let batch_allowed: Vec<HashMap<String, BTreeSet<u32>>> = batches
        .iter()
        .map(|batch| {
            batch
                .files
                .iter()
                .filter_map(|file| {
                    let path = normalize_path(&file.path);
                    let lines = allowed.get(&path)?.clone();
                    Some((path, lines))
                })
                .collect()
        })
        .collect();

    let mut candidates = Vec::new();
    let mut seen = HashSet::new();
    // Batch index → skip reason, for batches no agent reviewed.
    let mut unreviewed: BTreeMap<usize, &'static str> = BTreeMap::new();
    for task in tasks {
        let label = task.kind.label();
        match task.result {
            Ok(outcome) => {
                let Some(lines) = batch_allowed.get(task.batch) else {
                    continue;
                };
                for suggestion in keep_valid_batch(outcome.result, lines) {
                    let key = (
                        suggestion.file.clone(),
                        suggestion.line,
                        suggestion.title.clone(),
                    );
                    if seen.insert(key) {
                        candidates.push(CandidateFinding {
                            suggestion,
                            specialist: task.kind,
                        });
                    }
                }
            }
            Err(FinderFailure::TimedOut) => {
                push_once(warnings, format!("{label} reviewer did not finish in time"));
            }
            Err(FinderFailure::Agent(_)) if reviewed.contains(&task.batch) => {
                push_once(warnings, format!("{label} reviewer did not complete"));
            }
            Err(FinderFailure::Agent(err)) => {
                let reason = match err {
                    AgentError::Unparseable(_) => "unparseable",
                    _ => "llm_error",
                };
                unreviewed
                    .entry(task.batch)
                    .and_modify(|current| {
                        if *current != reason {
                            *current = "llm_error";
                        }
                    })
                    .or_insert(reason);
            }
        }
    }
    for (index, reason) in unreviewed {
        if let Some(batch) = batches.get(index) {
            tracing::warn!(batch = index, reason, "no finder agent reviewed a batch");
            skipped.extend(batch.files.iter().map(|file| (file.path.clone(), reason)));
        }
    }
    Ok(candidates)
}

/// The error when no Finder task succeeded. Retryable only when every failure was a
/// retryable chat error (`AllKeysFailed`, `NoKeys`, `Internal`); otherwise the first other
/// failure decides, and a review cut entirely by the deadline is `review timed out`.
fn all_failed(tasks: Vec<FinderTask>) -> EngineError {
    let mut retryable: Option<ChatError> = None;
    let mut fatal: Option<EngineError> = None;
    for task in tasks {
        let error = match task.result {
            Ok(_) => continue,
            Err(FinderFailure::TimedOut) => EngineError::Fatal("review timed out".into()),
            Err(FinderFailure::Agent(AgentError::Chat(err))) => match err {
                ChatError::AllKeysFailed { .. } | ChatError::NoKeys | ChatError::Internal(_) => {
                    retryable.get_or_insert(err);
                    continue;
                }
                other => chat_error(other),
            },
            Err(FinderFailure::Agent(AgentError::Unparseable(_))) => {
                EngineError::Fatal("LLM: review output could not be parsed".into())
            }
            Err(FinderFailure::Agent(AgentError::ToolsUnsupported(message))) => {
                chat_error(ChatError::ToolsUnsupported(message))
            }
        };
        fatal.get_or_insert(error);
    }
    match (fatal, retryable) {
        (Some(error), _) => error,
        (None, Some(err)) => chat_error(err),
        (None, None) => EngineError::Fatal("review timed out".into()),
    }
}

fn push_once(warnings: &mut Vec<String>, warning: String) {
    if !warnings.contains(&warning) {
        warnings.push(warning);
    }
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
            let allowed = commentable_lines(file.patch.as_deref().unwrap_or(""));
            Target { file, allowed }
        })
        .collect()
}

/// File dari commit baru yang masih ada di diff PR penuh. Baris yang boleh dikomentari harus
/// baru di commit ini DAN masih berubah di PR (supaya posisinya valid untuk provider).
fn incremental_targets(changed: Vec<FileDiff>, full: Vec<FileDiff>) -> Vec<Target> {
    let full_lines: HashMap<String, BTreeSet<u32>> = full
        .into_iter()
        .map(|file| {
            let lines = commentable_lines(file.patch.as_deref().unwrap_or(""));
            (file.path, lines)
        })
        .collect();
    changed
        .into_iter()
        .filter_map(|file| {
            let in_pr = full_lines.get(&file.path)?;
            let new_lines = commentable_lines(file.patch.as_deref().unwrap_or(""));
            let allowed = new_lines.intersection(in_pr).copied().collect();
            Some(Target { file, allowed })
        })
        .collect()
}

pub(crate) fn db_error(err: sqlx::Error) -> EngineError {
    EngineError::Retryable(format!("database: {err}"), None)
}

fn chat_error(err: ChatError) -> EngineError {
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
