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
    pub line: u32,
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
