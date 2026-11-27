//! Shared test harness for review integration tests.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use adus_core::crypto::Cipher;
use adus_core::types::ConnectionKind;
use adus_db::connections::{self, ConnectionStatus};
use adus_db::{repositories, settings};
use adus_git::{
    CloneSpec, FileDiff, GitError, GitProvider, GitProviderFactory, InlineComment, PatOwner,
    PostedComment, PrThread, PullRequest, RemoteRepo, RepoRef, ThreadRef, ThreadStatus,
};
use adus_llm::router::ChatService;
use adus_review::engine::{Engine, REVIEW_TIMEOUT, SPECIALISTS_REVIEW_TIMEOUT};
use adus_review::workspace::RepoCloner;
use async_trait::async_trait;
use sqlx::PgPool;
use uuid::Uuid;

/// base64 dari 32 byte nol (sama dengan `.cargo/config.toml`).
const TEST_KEY: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
pub const PAT: &str = "pat-secret-1234";

/// Satu `post_review` yang tercatat.
#[derive(Debug, Clone)]
pub struct PostedReview {
    pub comments: Vec<InlineComment>,
    pub summary: String,
}

/// Keadaan yang dibagi `FakeGit` dan semua provider yang dibuatnya.
pub struct GitState {
    pub pr: PullRequest,
    /// Hasil `pr_diff`.
    pub full: Vec<FileDiff>,
    /// Hasil `diff_between`.
    pub incremental: Vec<FileDiff>,
    /// Jika diisi, `get_pr` gagal dengan error ini.
    pub get_pr_error: Option<GitError>,
    /// `get_pr` menunggu selama ini sebelum menjawab (provider yang lambat).
    pub get_pr_delay: Duration,
    /// Jika diisi, `pr_diff` gagal dengan error ini.
    pub pr_diff_error: Option<GitError>,
    /// `pr_diff` waits this long before it answers (a slow provider).
    pub pr_diff_delay: Duration,
    /// Jika diisi, `diff_between` gagal dengan error ini.
    pub diff_between_error: Option<GitError>,
    /// Jika diisi, `post_review` gagal dengan error ini.
    pub post_error: Option<GitError>,
    /// Jika diisi, `post_comment` gagal dengan error ini.
    pub comment_error: Option<GitError>,
    /// `(kind, org_or_owner, token)` tiap `create`.
    pub created: Vec<(ConnectionKind, String, String)>,
    /// `(base, head)` tiap `diff_between`.
    pub diff_between_calls: Vec<(String, String)>,
    pub reviews: Vec<PostedReview>,
    pub comments: Vec<String>,
    /// Hasil `clone_spec` (`None`: provider tanpa clone, review diff-only).
    pub clone_spec: Option<CloneSpec>,
    /// Result of `list_pr_threads`.
    pub threads: Vec<PrThread>,
    /// When set, `list_pr_threads` fails with this error.
    pub threads_error: Option<GitError>,
    /// `(thread, body)` of each `reply_in_thread`.
    pub posted_threads: Vec<(ThreadRef, String)>,
    /// Each `set_thread_status`.
    pub statuses: Vec<(ThreadRef, ThreadStatus)>,
    /// Result of `get_thread` (`None`: `NotFound`).
    pub thread: Option<PrThread>,
}

/// Provider git palsu sekaligus pabriknya. Clone berbagi keadaan yang sama.
#[derive(Clone)]
pub struct FakeGit {
    state: Arc<Mutex<GitState>>,
}

impl FakeGit {
    pub fn new(pr: PullRequest, full: Vec<FileDiff>) -> FakeGit {
        FakeGit {
            state: Arc::new(Mutex::new(GitState {
                pr,
                full,
                incremental: Vec::new(),
                get_pr_error: None,
                get_pr_delay: Duration::ZERO,
                pr_diff_error: None,
                pr_diff_delay: Duration::ZERO,
                diff_between_error: None,
                post_error: None,
                comment_error: None,
                created: Vec::new(),
                diff_between_calls: Vec::new(),
                reviews: Vec::new(),
                comments: Vec::new(),
                clone_spec: None,
                threads: Vec::new(),
                threads_error: None,
                posted_threads: Vec::new(),
                statuses: Vec::new(),
                thread: None,
            })),
        }
    }

    pub fn state(&self) -> MutexGuard<'_, GitState> {
        self.state.lock().unwrap()
    }
}

impl GitProviderFactory for FakeGit {
    fn create(
        &self,
        kind: ConnectionKind,
        org_or_owner: &str,
        token: &str,
    ) -> Arc<dyn GitProvider> {
        self.state()
            .created
            .push((kind, org_or_owner.to_string(), token.to_string()));
        Arc::new(self.clone())
    }
}

#[async_trait]
impl GitProvider for FakeGit {
    async fn verify(&self) -> Result<PatOwner, GitError> {
        unimplemented!("not used by the engine")
    }

    async fn list_repos(&self) -> Result<Vec<RemoteRepo>, GitError> {
        unimplemented!("not used by the engine")
    }

    async fn ensure_webhooks(
        &self,
        _repo: &RepoRef,
        _url: &str,
        _secret: &str,
        _existing: &[String],
    ) -> Result<Vec<String>, GitError> {
        unimplemented!("not used by the engine")
    }

    async fn delete_webhooks(&self, _repo: &RepoRef, _ids: &[String]) -> Result<(), GitError> {
        unimplemented!("not used by the engine")
    }

    async fn webhooks_exist(
        &self,
        _repo: &RepoRef,
        _ids: &[String],
        _url: &str,
    ) -> Result<bool, GitError> {
        unimplemented!("not used by the engine")
    }

    async fn list_pr_threads(
        &self,
        _repo: &RepoRef,
        _pr_number: i64,
    ) -> Result<Vec<PrThread>, GitError> {
        let state = self.state();
        match &state.threads_error {
            Some(err) => Err(err.clone()),
            None => Ok(state.threads.clone()),
        }
    }

    async fn get_pr(&self, _repo: &RepoRef, _number: i64) -> Result<PullRequest, GitError> {
        let delay = self.state().get_pr_delay;
        tokio::time::sleep(delay).await;
        let state = self.state();
        match &state.get_pr_error {
            Some(err) => Err(err.clone()),
            None => Ok(state.pr.clone()),
        }
    }

    async fn pr_diff(&self, _repo: &RepoRef, _pr: &PullRequest) -> Result<Vec<FileDiff>, GitError> {
        let delay = self.state().pr_diff_delay;
        tokio::time::sleep(delay).await;
        let state = self.state();
        match &state.pr_diff_error {
            Some(err) => Err(err.clone()),
            None => Ok(state.full.clone()),
        }
    }

    async fn diff_between(
        &self,
        _repo: &RepoRef,
        base_sha: &str,
        head_sha: &str,
    ) -> Result<Vec<FileDiff>, GitError> {
        let mut state = self.state();
        state
            .diff_between_calls
            .push((base_sha.to_string(), head_sha.to_string()));
        match &state.diff_between_error {
            Some(err) => Err(err.clone()),
            None => Ok(state.incremental.clone()),
        }
    }

    async fn post_review(
        &self,
        _repo: &RepoRef,
        _pr: &PullRequest,
        comments: &[InlineComment],
        summary: &str,
    ) -> Result<Vec<PostedComment>, GitError> {
        let mut state = self.state();
        if let Some(err) = &state.post_error {
            return Err(err.clone());
        }
        let review = PostedReview {
            comments: comments.to_vec(),
            summary: summary.to_string(),
        };
        let count = review.comments.len();
        tracing::trace!(
            comments = count,
            summary_len = review.summary.len(),
            "fake post_review"
        );
        state.reviews.push(review);
        Ok((0..count)
            .map(|i| PostedComment {
                index: i,
                thread_id: Some(format!("t{i}")),
                comment_id: format!("c{i}"),
            })
            .collect())
    }

    /// Returns `r{n}`, `n` = replies so far.
    async fn reply_in_thread(
        &self,
        _repo: &RepoRef,
        _pr_number: i64,
        thread: &ThreadRef,
        body: &str,
    ) -> Result<String, GitError> {
        let mut state = self.state();
        state
            .posted_threads
            .push((thread.clone(), body.to_string()));
        Ok(format!("r{}", state.posted_threads.len() - 1))
    }

    async fn set_thread_status(
        &self,
        _repo: &RepoRef,
        _pr_number: i64,
        thread: &ThreadRef,
        status: ThreadStatus,
    ) -> Result<(), GitError> {
        self.state().statuses.push((thread.clone(), status));
        Ok(())
    }

    async fn get_thread(
        &self,
        _repo: &RepoRef,
        _pr_number: i64,
        _thread: &ThreadRef,
    ) -> Result<PrThread, GitError> {
        self.state().thread.clone().ok_or(GitError::NotFound)
    }

    async fn post_comment(
        &self,
        _repo: &RepoRef,
        _pr_number: i64,
        body: &str,
    ) -> Result<(), GitError> {
        let mut state = self.state();
        if let Some(err) = &state.comment_error {
            return Err(err.clone());
        }
        state.comments.push(body.to_string());
        Ok(())
    }

    fn clone_spec(&self, _repo: &RepoRef, _pr: &PullRequest) -> Option<CloneSpec> {
        self.state().clone_spec.clone()
    }
}

/// Database seeded with a GitHub connection and one repository.
pub struct Setup {
    pub engine: Engine,
    pub git: FakeGit,
    pub connection_id: Uuid,
    pub repository_id: Uuid,
}

pub async fn setup(
    pool: &PgPool,
    git: FakeGit,
    chat: Arc<dyn ChatService>,
    cloner: Arc<dyn RepoCloner>,
) -> Setup {
    let cipher = Cipher::from_base64_key(TEST_KEY).unwrap();
    let conn = connections::insert(
        pool,
        ConnectionKind::Github,
        "gh",
        "acme",
        &cipher.encrypt(PAT),
        "1234",
        "adudu",
    )
    .await
    .unwrap();
    let repo = repositories::NewRepository {
        connection_id: conn.id,
        external_id: "42",
        owner: "acme",
        project: None,
        project_name: None,
        name: "api",
        default_branch: Some("main"),
        webhook_token: "t",
    };
    let repo = repositories::insert(pool, repo).await.unwrap().unwrap();
    let mut current = settings::get(pool).await.unwrap();
    // The small-PR path is opt-in per test: most tests use tiny diffs and expect the full path.
    current.small_pr_max_lines = 0;
    settings::update(pool, &current).await.unwrap();

    Setup {
        engine: Engine {
            db: pool.clone(),
            cipher: Arc::new(cipher),
            git: Arc::new(git.clone()),
            chat,
            cloner,
            review_timeout: REVIEW_TIMEOUT,
            specialists_review_timeout: SPECIALISTS_REVIEW_TIMEOUT,
        },
        git,
        connection_id: conn.id,
        repository_id: repo.id,
    }
}

pub fn pr(head_sha: &str) -> PullRequest {
    PullRequest {
        number: 7,
        title: "Add login".into(),
        description: "".into(),
        head_sha: head_sha.into(),
        base_sha: "base".into(),
        is_draft: false,
        source_ref: None,
    }
}

pub async fn conn_status(pool: &PgPool, connection_id: Uuid) -> ConnectionStatus {
    connections::get(pool, connection_id)
        .await
        .unwrap()
        .unwrap()
        .status
}
