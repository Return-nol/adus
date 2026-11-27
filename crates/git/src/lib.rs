pub mod azure;
pub mod diff;
pub mod github;
pub mod types;

use std::sync::Arc;

use async_trait::async_trait;

pub use adus_core::types::{ConnectionKind, normalize_path};
pub use azure::AzureProvider;
pub use github::GithubProvider;
pub use types::{
    CloneSpec, FileDiff, FileStatus, GitError, InlineComment, PatOwner, PostedComment, PrThread,
    PrThreadComment, PullRequest, RemoteRepo, RepoRef, ThreadRef, ThreadStatus,
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
    /// Read-only: GET tiap id. `true` hanya jika semua masih ada, aktif, dan mengarah ke `url`
    /// (404, nonaktif, atau URL lain → `false`).
    async fn webhooks_exist(
        &self,
        repo: &RepoRef,
        ids: &[String],
        url: &str,
    ) -> Result<bool, GitError>;
    async fn get_pr(&self, repo: &RepoRef, number: i64) -> Result<PullRequest, GitError>;
    async fn pr_diff(&self, repo: &RepoRef, pr: &PullRequest) -> Result<Vec<FileDiff>, GitError>;
    async fn diff_between(
        &self,
        repo: &RepoRef,
        base_sha: &str,
        head_sha: &str,
    ) -> Result<Vec<FileDiff>, GitError>;
    /// One review/batch of inline comments + one summary comment. Returns the provider ids of
    /// the inline comments it could identify (its length is the posted count).
    async fn post_review(
        &self,
        repo: &RepoRef,
        pr: &PullRequest,
        comments: &[InlineComment],
        summary: &str,
    ) -> Result<Vec<PostedComment>, GitError>;
    /// Replies inside a thread. Returns the new comment id.
    async fn reply_in_thread(
        &self,
        repo: &RepoRef,
        pr_number: i64,
        thread: &ThreadRef,
        body: &str,
    ) -> Result<String, GitError>;
    /// Sets the thread status. GitHub has only resolved/unresolved.
    async fn set_thread_status(
        &self,
        repo: &RepoRef,
        pr_number: i64,
        thread: &ThreadRef,
        status: ThreadStatus,
    ) -> Result<(), GitError>;
    /// The current comments of one thread, oldest first.
    async fn get_thread(
        &self,
        repo: &RepoRef,
        pr_number: i64,
        thread: &ThreadRef,
    ) -> Result<PrThread, GitError>;
    async fn post_comment(
        &self,
        repo: &RepoRef,
        pr_number: i64,
        body: &str,
    ) -> Result<(), GitError>;
    /// Discussion threads of a PR (inline threads and top-level comments), comments oldest first.
    async fn list_pr_threads(
        &self,
        repo: &RepoRef,
        pr_number: i64,
    ) -> Result<Vec<PrThread>, GitError>;
    /// How to shallow-fetch the PR head with `git`. `None` when the provider cannot build one
    /// (the review then falls back to diff-only).
    fn clone_spec(&self, _repo: &RepoRef, _pr: &PullRequest) -> Option<CloneSpec> {
        None
    }
}

/// Builds `{base}/{segments...}` with every segment percent-encoded. `None` if `base` is not a
/// usable base URL or any segment is empty, `.` or `..`.
pub(crate) fn git_url(base: &str, segments: &[&str]) -> Option<String> {
    if segments
        .iter()
        .any(|s| s.is_empty() || *s == "." || *s == "..")
    {
        return None;
    }
    let mut url = reqwest::Url::parse(base).ok()?;
    url.path_segments_mut()
        .ok()?
        .pop_if_empty()
        .extend(segments);
    Some(url.to_string())
}

/// `Basic base64(user:password)`.
pub(crate) fn basic_auth_header(user: &str, password: &str) -> String {
    use base64::Engine;
    let encoded = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}"));
    format!("Basic {encoded}")
}

/// Longest serde message kept in a `GitError::Decode`.
const DECODE_DETAIL_CHARS: usize = 200;

/// Decode a JSON response body into `T`. A failed body read is `GitError::Http`; a body that
/// does not match `T` is `GitError::Decode` carrying serde's message (field and position, e.g.
/// "missing field `title` at line 1 column 42"), cut to `DECODE_DETAIL_CHARS`, so a provider
/// shape change names the field in the logs.
pub(crate) async fn decode<T: serde::de::DeserializeOwned>(
    resp: reqwest::Response,
) -> Result<T, GitError> {
    let body = resp
        .bytes()
        .await
        .map_err(|e| GitError::Http(e.without_url().to_string()))?;
    serde_json::from_slice(&body).map_err(|e| {
        let detail: String = e.to_string().chars().take(DECODE_DETAIL_CHARS).collect();
        GitError::Decode(detail)
    })
}

pub trait GitProviderFactory: Send + Sync {
    fn create(&self, kind: ConnectionKind, org_or_owner: &str, token: &str)
    -> Arc<dyn GitProvider>;
}

/// Pabrik provider asli: membuat `GithubProvider` / `AzureProvider` per token.
pub struct HttpGitProviderFactory {
    pub http: reqwest::Client,
    pub github_base: String,
    pub azure_base: String,
}

impl HttpGitProviderFactory {
    /// Base URL publik: `https://api.github.com` dan `https://dev.azure.com`.
    pub fn new(http: reqwest::Client) -> Self {
        Self {
            http,
            github_base: "https://api.github.com".into(),
            azure_base: "https://dev.azure.com".into(),
        }
    }
}

impl GitProviderFactory for HttpGitProviderFactory {
    /// `org_or_owner` dipakai Azure sebagai organization; GitHub tidak memakainya.
    fn create(
        &self,
        kind: ConnectionKind,
        org_or_owner: &str,
        token: &str,
    ) -> Arc<dyn GitProvider> {
        match kind {
            ConnectionKind::Github => Arc::new(GithubProvider::new(
                self.http.clone(),
                self.github_base.clone(),
                token.to_string(),
            )),
            ConnectionKind::AzureDevops => Arc::new(AzureProvider::new(
                self.http.clone(),
                self.azure_base.clone(),
                org_or_owner.to_string(),
                token.to_string(),
            )),
        }
    }
}
