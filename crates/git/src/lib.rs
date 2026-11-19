pub mod diff;
pub mod github;
pub mod types;

use std::sync::Arc;

use async_trait::async_trait;

pub use adus_core::types::{ConnectionKind, normalize_path};
pub use github::GithubProvider;
pub use types::{
    FileDiff, FileStatus, GitError, InlineComment, PatOwner, PullRequest, RemoteRepo, RepoRef,
};

#[async_trait]
pub trait GitProvider: Send + Sync {
    async fn verify(&self) -> Result<PatOwner, GitError>;
    async fn list_repos(&self) -> Result<Vec<RemoteRepo>, GitError>;
    /// Buat webhook yang belum ada. `existing` = id yang tersimpan; return id lengkap setelah sinkron.
    async fn ensure_webhooks(
        &self,
        repo: &RepoRef,
        url: &str,
        secret: &str,
        existing: &[String],
    ) -> Result<Vec<String>, GitError>;
    async fn delete_webhooks(&self, repo: &RepoRef, ids: &[String]) -> Result<(), GitError>;
    async fn get_pr(&self, repo: &RepoRef, number: i64) -> Result<PullRequest, GitError>;
    async fn pr_diff(&self, repo: &RepoRef, pr: &PullRequest) -> Result<Vec<FileDiff>, GitError>;
    async fn diff_between(
        &self,
        repo: &RepoRef,
        base_sha: &str,
        head_sha: &str,
    ) -> Result<Vec<FileDiff>, GitError>;
    /// Satu review/batch komentar inline + satu komentar ringkasan. Return jumlah inline yang terposting.
    async fn post_review(
        &self,
        repo: &RepoRef,
        pr: &PullRequest,
        comments: &[InlineComment],
        summary: &str,
    ) -> Result<usize, GitError>;
    async fn post_comment(
        &self,
        repo: &RepoRef,
        pr_number: i64,
        body: &str,
    ) -> Result<(), GitError>;
}

pub trait GitProviderFactory: Send + Sync {
    fn create(&self, kind: ConnectionKind, org_or_owner: &str, token: &str)
    -> Arc<dyn GitProvider>;
}

/// Pabrik provider asli. `impl GitProviderFactory` ditambahkan di Task 8-9 saat GitHub/Azure ada.
pub struct HttpGitProviderFactory {
    pub http: reqwest::Client,
    pub github_base: String,
    pub azure_base: String,
}
