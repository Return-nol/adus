//! Mesin review: ambil diff PR, minta LLM mereview per file, posting temuan, catat di `reviews`.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::time::Duration;

use adus_core::crypto::Cipher;
use adus_core::job::Trigger;
use adus_core::types::{ConnectionKind, Severity};
use adus_db::connections::{self, Connection, ConnectionStatus};
use adus_db::repositories::{self, Repository};
use adus_db::reviews::{self, NewReview, ReviewStatus};
use adus_db::{memories, rules, settings};
use adus_git::diff::{added_line_count, commentable_lines};
use adus_git::{
    FileDiff, FileStatus, GitError, GitProvider, GitProviderFactory, InlineComment, PullRequest,
    RepoRef,
};
use adus_llm::ChatRequest;
use adus_llm::router::{ChatError, ChatService, UsageContext};
use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    MARKER, Suggestion, build_prompt, format_inline, format_summary, keep_valid, parse_suggestions,
};

const MAX_OUTPUT_TOKENS: u32 = 4096;

pub struct Engine {
    pub db: PgPool,
    pub cipher: Arc<Cipher>,
    pub git: Arc<dyn GitProviderFactory>,
    pub chat: Arc<dyn ChatService>,
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

impl Engine {
    /// Review satu PR. Return id baris `reviews`, atau `Uuid::nil()` jika repo tidak ada
    /// (tidak ada repo untuk dicatat). Setiap hasil lain — selesai, dilewati, gagal — punya baris.
    pub async fn review_pr(
        &self,
        repository_id: Uuid,
        pr_number: i64,
        trigger: Trigger,
    ) -> Result<Uuid, EngineError> {
        let Some(repo) = repositories::get(&self.db, repository_id)
            .await
            .map_err(db_error)?
        else {
            return Ok(Uuid::nil());
        };
        let new = NewReview {
            repository_id,
            pr_number,
            trigger,
        };
        let review_id = reviews::insert(&self.db, new).await.map_err(db_error)?.id;

        let (status, posted, skip_reason) =
            match self.run(&repo, review_id, pr_number, trigger).await {
                Ok(Outcome::Done { posted }) => (ReviewStatus::Done, posted, None),
                Ok(Outcome::Skipped(reason)) => (ReviewStatus::Skipped, 0, Some(reason)),
                Err(err) => {
                    let message = err.to_string();
                    // Error aslinya lebih penting daripada gagal mencatat; baris tetap queued/running.
                    let _ = reviews::finish(
                        &self.db,
                        review_id,
                        ReviewStatus::Failed,
                        0,
                        Some(&message),
                        None,
                    )
                    .await;
                    return Err(err);
                }
            };
        let posted = i32::try_from(posted).unwrap_or(i32::MAX);
        reviews::finish(&self.db, review_id, status, posted, None, skip_reason)
            .await
            .map_err(db_error)?;
        Ok(review_id)
    }

    async fn run(
        &self,
        repo: &Repository,
        review_id: Uuid,
        pr_number: i64,
        trigger: Trigger,
    ) -> Result<Outcome, EngineError> {
        if !repo.enabled {
            return Ok(Outcome::Skipped("repository_disabled"));
        }
        if trigger != Trigger::Command && !repo.auto_review {
            return Ok(Outcome::Skipped("auto_review_off"));
        }

        let conn = connections::get(&self.db, repo.connection_id)
            .await
            .map_err(db_error)?
            .ok_or_else(|| EngineError::Fatal("connection not found".into()))?;
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
        let model = match (&repo.review_provider_id, &repo.review_model) {
            (Some(provider), Some(model)) => Some((*provider, model.clone())),
            _ => settings
                .default_provider_id
                .zip(settings.default_model.clone()),
        };
        let Some((provider_id, model)) = model else {
            let body = format!(
                "{MARKER}\nAdudu can't review yet: no AI model is configured. \
                 An admin can set one under Settings."
            );
            // Gagal posting tidak mengubah hasilnya: review tetap tidak bisa jalan.
            let _ = git.post_comment(&repo_ref, pr_number, &body).await;
            return Err(EngineError::Fatal("no review model configured".into()));
        };

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
        let candidates = match last {
            Some(last_head) => {
                self.incremental(&*git, &conn, &repo_ref, &pr, &last_head)
                    .await?
            }
            None => self
                .fetch(&conn, git.pr_diff(&repo_ref, &pr).await)
                .await?
                .into_iter()
                .map(|file| {
                    let allowed = commentable_lines(file.patch.as_deref().unwrap_or(""));
                    Target { file, allowed }
                })
                .collect(),
        };

        let max_files = usize::try_from(settings.max_files_per_review).unwrap_or(0);
        let max_lines = usize::try_from(settings.max_patch_lines_per_file).unwrap_or(0);
        let mut targets = Vec::new();
        let mut skipped: Vec<(String, &'static str)> = Vec::new();
        for target in candidates {
            if target.file.status == FileStatus::Deleted {
                continue;
            }
            let Some(patch) = &target.file.patch else {
                skipped.push((target.file.path, "binary"));
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

        let rules = rules::applicable(&self.db, repo.id)
            .await
            .map_err(db_error)?;
        let memories = memories::applicable(&self.db, repo.id)
            .await
            .map_err(db_error)?;
        let ctx = UsageContext {
            review_id: Some(review_id),
            repository_id: Some(repo.id),
        };
        let mut found: Vec<Suggestion> = Vec::new();
        // ponytail: file direview berurutan; tambah concurrency terbatas jika review terasa lambat.
        for target in targets {
            let (system, user) = build_prompt(&target.file, &rules, &memories, &pr);
            let mut parsed = None;
            for _attempt in 0..2 {
                let req = ChatRequest {
                    model: model.clone(),
                    system: system.clone(),
                    user: user.clone(),
                    max_tokens: MAX_OUTPUT_TOKENS,
                };
                let response = self
                    .chat
                    .chat(provider_id, req, ctx.clone())
                    .await
                    .map_err(chat_error)?;
                if let Ok(suggestions) = parse_suggestions(&response.text) {
                    parsed = Some(suggestions);
                    break;
                }
            }
            match parsed {
                Some(suggestions) => {
                    found.extend(keep_valid(suggestions, &target.file, &target.allowed));
                }
                None => skipped.push((target.file.path, "unparseable")),
            }
        }

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
        let summary = format_summary(&counts, &skipped);
        match git.post_review(&repo_ref, &pr, &comments, &summary).await {
            Ok(posted) => Ok(Outcome::Done { posted }),
            Err(err) => Err(self.post_error(&conn, err).await),
        }
    }

    /// File dari commit baru yang masih ada di diff PR penuh. Baris yang boleh dikomentari harus
    /// baru di commit ini DAN masih berubah di PR (supaya posisinya valid untuk provider).
    async fn incremental(
        &self,
        git: &dyn GitProvider,
        conn: &Connection,
        repo_ref: &RepoRef,
        pr: &PullRequest,
        last_head: &str,
    ) -> Result<Vec<Target>, EngineError> {
        let changed = self
            .fetch(
                conn,
                git.diff_between(repo_ref, last_head, &pr.head_sha).await,
            )
            .await?;
        let full = self.fetch(conn, git.pr_diff(repo_ref, pr).await).await?;
        let full_lines: HashMap<String, BTreeSet<u32>> = full
            .into_iter()
            .map(|file| {
                let lines = commentable_lines(file.patch.as_deref().unwrap_or(""));
                (file.path, lines)
            })
            .collect();
        Ok(changed
            .into_iter()
            .filter_map(|file| {
                let in_pr = full_lines.get(&file.path)?;
                let new_lines = commentable_lines(file.patch.as_deref().unwrap_or(""));
                let allowed = new_lines.intersection(in_pr).copied().collect();
                Some(Target { file, allowed })
            })
            .collect())
    }

    /// Hasil pengambilan data dari provider git; error dipetakan oleh `fetch_error`.
    async fn fetch<T>(
        &self,
        conn: &Connection,
        result: Result<T, GitError>,
    ) -> Result<T, EngineError> {
        match result {
            Ok(value) => Ok(value),
            Err(err) => Err(self.fetch_error(conn, err).await),
        }
    }

    /// Saat mengambil data, semua error selain token ditolak dianggap sementara.
    async fn fetch_error(&self, conn: &Connection, err: GitError) -> EngineError {
        match err {
            GitError::Unauthorized => self.unauthorized(conn).await,
            other => EngineError::Retryable(format!("git provider: {other}"), None),
        }
    }

    /// GitHub memposting review dalam satu request, jadi aman diulang. Azure memposting thread
    /// satu per satu: mengulang akan menggandakan thread yang sudah terposting, jadi selalu fatal.
    async fn post_error(&self, conn: &Connection, err: GitError) -> EngineError {
        let message = format!("posting the review failed: {err}");
        match (err, conn.kind) {
            (GitError::Unauthorized, _) => self.unauthorized(conn).await,
            (GitError::RateLimited | GitError::Http(_), ConnectionKind::Github) => {
                EngineError::Retryable(message, None)
            }
            _ => EngineError::Fatal(message),
        }
    }

    /// PAT ditolak: tandai connection `invalid` supaya admin melihatnya di dashboard.
    async fn unauthorized(&self, conn: &Connection) -> EngineError {
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

fn db_error(err: sqlx::Error) -> EngineError {
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
        ChatError::Invalid(_) => EngineError::Fatal(format!("LLM: {err}")),
    }
}
