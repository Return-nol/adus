//! Fake git dan LLM untuk test engine, plus helper seed database.
#![allow(dead_code)]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};

use adus_core::crypto::Cipher;
use adus_core::types::{ConnectionKind, ProviderKind};
use adus_db::{ai, connections, repositories, settings};
use adus_git::{
    FileDiff, FileStatus, GitError, GitProvider, GitProviderFactory, InlineComment, PatOwner,
    PullRequest, RemoteRepo, RepoRef,
};
use adus_llm::router::{ChatError, ChatService, UsageContext};
use adus_llm::{ChatRequest, ChatResponse};
use adus_review::engine::Engine;
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
    /// Jika diisi, `pr_diff` gagal dengan error ini.
    pub pr_diff_error: Option<GitError>,
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
                pr_diff_error: None,
                diff_between_error: None,
                post_error: None,
                comment_error: None,
                created: Vec::new(),
                diff_between_calls: Vec::new(),
                reviews: Vec::new(),
                comments: Vec::new(),
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

    async fn get_pr(&self, _repo: &RepoRef, _number: i64) -> Result<PullRequest, GitError> {
        let state = self.state();
        match &state.get_pr_error {
            Some(err) => Err(err.clone()),
            None => Ok(state.pr.clone()),
        }
    }

    async fn pr_diff(&self, _repo: &RepoRef, _pr: &PullRequest) -> Result<Vec<FileDiff>, GitError> {
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
    ) -> Result<usize, GitError> {
        let mut state = self.state();
        if let Some(err) = &state.post_error {
            return Err(err.clone());
        }
        state.reviews.push(PostedReview {
            comments: comments.to_vec(),
            summary: summary.to_string(),
        });
        Ok(comments.len())
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
}

/// Satu panggilan `chat` yang tercatat.
#[derive(Debug, Clone)]
pub struct ChatCall {
    pub provider_id: Uuid,
    pub model: String,
    pub user: String,
    pub max_tokens: u32,
    pub ctx: UsageContext,
}

/// LLM palsu: jawaban diambil berurutan dari `script`; habis → `[]`.
#[derive(Default)]
pub struct FakeChat {
    script: Mutex<VecDeque<Result<String, ChatError>>>,
    calls: Mutex<Vec<ChatCall>>,
}

impl FakeChat {
    pub fn push(&self, result: Result<&str, ChatError>) {
        self.script
            .lock()
            .unwrap()
            .push_back(result.map(str::to_string));
    }

    pub fn calls(&self) -> Vec<ChatCall> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl ChatService for FakeChat {
    async fn chat(
        &self,
        provider_id: Uuid,
        req: ChatRequest,
        ctx: UsageContext,
    ) -> Result<ChatResponse, ChatError> {
        self.calls.lock().unwrap().push(ChatCall {
            provider_id,
            model: req.model,
            user: req.user,
            max_tokens: req.max_tokens,
            ctx,
        });
        let next = self.script.lock().unwrap().pop_front();
        next.unwrap_or(Ok("[]".to_string()))
            .map(|text| ChatResponse {
                text,
                input_tokens: 1,
                output_tokens: 1,
            })
    }
}

/// Database yang sudah berisi connection GitHub, satu repo, dan model default.
pub struct Setup {
    pub engine: Engine,
    pub git: FakeGit,
    pub chat: Arc<FakeChat>,
    pub connection_id: Uuid,
    pub repository_id: Uuid,
    pub provider_id: Uuid,
}

pub async fn setup(pool: &PgPool, git: FakeGit) -> Setup {
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
        name: "api",
        default_branch: Some("main"),
        webhook_token: "t",
    };
    let repo = repositories::insert(pool, repo).await.unwrap().unwrap();
    let provider = ai::insert_provider(pool, ProviderKind::Openai, "openai", None)
        .await
        .unwrap();
    let mut current = settings::get(pool).await.unwrap();
    current.default_provider_id = Some(provider.id);
    current.default_model = Some("gpt-test".into());
    settings::update(pool, &current).await.unwrap();

    let chat = Arc::new(FakeChat::default());
    Setup {
        engine: Engine {
            db: pool.clone(),
            cipher: Arc::new(cipher),
            git: Arc::new(git.clone()),
            chat: chat.clone(),
        },
        git,
        chat,
        connection_id: conn.id,
        repository_id: repo.id,
        provider_id: provider.id,
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
    }
}

pub fn file(path: &str, patch: &str) -> FileDiff {
    FileDiff {
        path: path.into(),
        old_path: None,
        status: FileStatus::Modified,
        patch: Some(patch.into()),
    }
}

/// Jawaban LLM berisi satu temuan.
pub fn finding(path: &str, line: u32, title: &str) -> String {
    format!(
        r#"[{{"file":"{path}","line":{line},"severity":"warning","title":"{title}","body":"why"}}]"#
    )
}

pub fn redis_url() -> String {
    std::env::var("REDIS_URL").expect("REDIS_URL must be set (see .cargo/config.toml)")
}

/// Antrian dengan prefix unik `test-{uuid}`; panggil `cleanup_queue` di akhir test.
pub async fn test_queue() -> (adus_queue::Queue, String) {
    let prefix = format!("test-{}", Uuid::new_v4());
    let queue = adus_queue::Queue::connect(&redis_url(), &prefix)
        .await
        .unwrap();
    (queue, prefix)
}

/// Hapus semua key Redis milik prefix test.
pub async fn cleanup_queue(prefix: &str) {
    use redis::AsyncCommands;
    let client = redis::Client::open(redis_url()).unwrap();
    let mut conn = client.get_multiplexed_async_connection().await.unwrap();
    let keys: Vec<String> = conn.keys(format!("{prefix}:*")).await.unwrap();
    if !keys.is_empty() {
        let _: () = conn.del(keys).await.unwrap();
    }
}

/// Semua job di stream antrian test, urut masuk.
pub async fn queued_jobs(prefix: &str) -> Vec<adus_core::job::Job> {
    let client = redis::Client::open(redis_url()).unwrap();
    let mut conn = client.get_multiplexed_async_connection().await.unwrap();
    let entries: Vec<(String, std::collections::HashMap<String, String>)> = redis::cmd("XRANGE")
        .arg(format!("{prefix}:jobs"))
        .arg("-")
        .arg("+")
        .query_async(&mut conn)
        .await
        .unwrap();
    entries
        .iter()
        .map(|(_, fields)| serde_json::from_str(&fields["job"]).unwrap())
        .collect()
}
