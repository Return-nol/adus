use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Pemilik PAT hasil `verify`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PatOwner {
    pub id: String,
    pub login: String,
    pub email: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RepoRef {
    pub external_id: String,
    /// GitHub owner atau Azure organization.
    pub owner: String,
    /// Hanya Azure.
    pub project: Option<String>,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RemoteRepo {
    pub r#ref: RepoRef,
    pub default_branch: Option<String>,
    /// Azure: nama project (untuk label `project/repo`). GitHub: `None`.
    pub project_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PullRequest {
    pub number: i64,
    pub title: String,
    pub description: String,
    pub head_sha: String,
    /// Merge base.
    pub base_sha: String,
    pub is_draft: bool,
    /// Source branch ref of the PR (Azure `sourceRefName`, e.g. `refs/heads/feature`).
    /// GitHub: `None`.
    pub source_ref: Option<String>,
}

/// Everything needed to shallow-fetch a PR head with `git`.
#[derive(Clone)]
pub struct CloneSpec {
    pub url: String,
    pub refspec: String,
    /// Tried once when fetching `refspec` fails for a non-auth reason (Azure: the source branch ref).
    pub fallback_refspec: Option<String>,
    pub expected_sha: String,
    /// Value for the `Authorization` HTTP header (e.g. `Basic <base64>`). Never embedded in
    /// `url`, argv or `.git/config`.
    pub auth_header: String,
}

/// Hand-written so the auth header never ends up in a log line through `{:?}`.
impl std::fmt::Debug for CloneSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CloneSpec")
            .field("url", &self.url)
            .field("refspec", &self.refspec)
            .field("fallback_refspec", &self.fallback_refspec)
            .field("expected_sha", &self.expected_sha)
            .field("auth_header", &"[redacted]")
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FileDiff {
    /// Tanpa `/` di depan.
    pub path: String,
    pub old_path: Option<String>,
    pub status: FileStatus,
    /// `None`: biner atau terlalu besar.
    pub patch: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct InlineComment {
    pub path: String,
    /// Inclusive first line for multi-line comments (must be `< line` and in the same diff hunk; `None` = single-line).
    pub start_line: Option<u32>,
    pub line: u32,
    /// 1-based UTF-16 end column offset on `line` for multi-line Azure DevOps threads (`None`
    /// defaults to `1`). Azure only: the engine sets it for every provider, GitHub ignores it.
    pub end_offset: Option<u32>,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum GitError {
    #[error("unauthorized")]
    Unauthorized,
    #[error("not found")]
    NotFound,
    #[error("rate limited")]
    RateLimited,
    /// Gagal sebelum ada jawaban (jaringan, timeout).
    #[error("http error: {0}")]
    Http(String),
    /// Provider menjawab dengan status gagal yang tidak punya varian sendiri.
    #[error("unexpected status {0}")]
    Status(u16),
    #[error("decode error: {0}")]
    Decode(String),
}
