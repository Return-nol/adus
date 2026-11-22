//! Fake git dan LLM untuk test engine, plus helper seed database.
#![allow(dead_code)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use adus_core::crypto::Cipher;
use adus_core::types::{ConnectionKind, ProviderKind};
use adus_db::{ai, connections, repositories, settings};
use adus_git::{
    CloneSpec, FileDiff, FileStatus, GitError, GitProvider, GitProviderFactory, InlineComment,
    PatOwner, PullRequest, RemoteRepo, RepoRef,
};
use adus_llm::router::{ChatError, ChatService, UsageContext};
use adus_llm::{
    ChatRequest, ChatResponse, Message, ReasoningEffort, StopReason, ToolCall, UserPart,
};
use adus_review::engine::{Engine, REVIEW_TIMEOUT, SPECIALISTS_REVIEW_TIMEOUT};
use adus_review::workspace::{RepoCloner, RepoWorkspace, WorkspaceError};
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
                diff_between_error: None,
                post_error: None,
                comment_error: None,
                created: Vec::new(),
                diff_between_calls: Vec::new(),
                reviews: Vec::new(),
                comments: Vec::new(),
                clone_spec: None,
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

    fn clone_spec(&self, _repo: &RepoRef, _pr: &PullRequest) -> Option<CloneSpec> {
        self.state().clone_spec.clone()
    }
}

/// Which review stage sent a chat request, read from its system prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Stage {
    Finder,
    Verifier,
    Dedup,
    Other,
}

fn stage_of(system: &str) -> Stage {
    if system.contains("<ReviewInstructions>") {
        Stage::Finder
    } else if system.contains("Adudu's verifier") {
        Stage::Verifier
    } else if system.contains("You merge duplicate findings") {
        Stage::Dedup
    } else {
        Stage::Other
    }
}

/// One scripted model answer.
#[derive(Debug, Clone)]
pub enum Reply {
    Text(String),
    /// Tool calls `(name, arguments)`, no text.
    Tools(Vec<(String, serde_json::Value)>),
    Err(ChatError),
}

impl Reply {
    pub fn text(text: &str) -> Reply {
        Reply::Text(text.to_string())
    }

    /// One `submit_findings` call carrying `findings` (a JSON array).
    pub fn submit(findings: serde_json::Value) -> Reply {
        Reply::Tools(vec![(
            "submit_findings".into(),
            serde_json::json!({ "findings": findings }),
        )])
    }

    pub fn tool(name: &str, args: serde_json::Value) -> Reply {
        Reply::Tools(vec![(name.to_string(), args)])
    }
}

/// A scripted answer for `stage`, used by the first call whose system prompt or first user
/// message contains `needle` (any call of that stage when `None`).
struct Scripted {
    stage: Stage,
    needle: Option<String>,
    reply: Reply,
}

/// Satu panggilan `chat` yang tercatat.
#[derive(Debug, Clone)]
pub struct ChatCall {
    pub stage: Stage,
    pub provider_id: Uuid,
    pub model: String,
    pub system: String,
    /// The first user message (the prompt).
    pub user: String,
    /// Names of the tools offered.
    pub tools: Vec<String>,
    /// Contents of every tool result in the conversation, in order.
    pub tool_results: Vec<String>,
    pub max_tokens: u32,
    pub effort: Option<ReasoningEffort>,
    pub ctx: UsageContext,
}

/// A Finder agent that keeps reading `read_path` while the repository tools are offered and
/// submits `findings` once only `submit_findings` is (what a model does after the wrap-up
/// reminder). Its first call takes `first_delay`, every later call `delay`.
struct Explorer {
    needle: String,
    read_path: String,
    findings: serde_json::Value,
    first_delay: Duration,
    delay: Duration,
    calls: usize,
}

/// LLM palsu. Jawaban diambil dari skrip: entri pertama untuk stage itu yang `needle`-nya cocok.
/// Tanpa entri: Finder `[]`, Verifier `{"keep": true}`, Dedup jawaban tak valid (semua
/// dipertahankan). Panggilan Finder yang memuat `needle` dari `delay` dijawab setelah jeda itu
/// (urutan pengambilan jawaban tetap urutan panggilan).
#[derive(Default)]
pub struct FakeChat {
    script: Mutex<Vec<Scripted>>,
    calls: Mutex<Vec<ChatCall>>,
    delays: Mutex<Vec<(String, Duration)>>,
    /// Per stage: (running now, most at once).
    in_flight: Mutex<HashMap<Stage, (usize, usize)>>,
    next_call_id: Mutex<usize>,
    explorers: Mutex<Vec<Explorer>>,
}

impl FakeChat {
    /// A Finder answer for any batch and specialist.
    pub fn push(&self, result: Result<&str, ChatError>) {
        let reply = match result {
            Ok(text) => Reply::text(text),
            Err(err) => Reply::Err(err),
        };
        self.script_for(Stage::Finder, None, reply);
    }

    /// An answer for `stage`, for the first call that contains `needle`.
    pub fn push_for(&self, stage: Stage, needle: &str, reply: Reply) {
        self.script_for(stage, Some(needle), reply);
    }

    /// An answer for any call of `stage`.
    pub fn push_stage(&self, stage: Stage, reply: Reply) {
        self.script_for(stage, None, reply);
    }

    fn script_for(&self, stage: Stage, needle: Option<&str>, reply: Reply) {
        self.script.lock().unwrap().push(Scripted {
            stage,
            needle: needle.map(str::to_string),
            reply,
        });
    }

    pub fn calls(&self) -> Vec<ChatCall> {
        self.calls.lock().unwrap().clone()
    }

    pub fn calls_of(&self, stage: Stage) -> Vec<ChatCall> {
        self.calls()
            .into_iter()
            .filter(|c| c.stage == stage)
            .collect()
    }

    pub fn finder_calls(&self) -> Vec<ChatCall> {
        self.calls_of(Stage::Finder)
    }

    pub fn verifier_calls(&self) -> Vec<ChatCall> {
        self.calls_of(Stage::Verifier)
    }

    pub fn dedup_calls(&self) -> Vec<ChatCall> {
        self.calls_of(Stage::Dedup)
    }

    /// Finder calls that contain `needle` (e.g. `=== a.rs (` or a role line) answer after `delay`.
    pub fn delay(&self, needle: &str, delay: Duration) {
        self.delays
            .lock()
            .unwrap()
            .push((needle.to_string(), delay));
    }

    /// Every Finder call containing `needle` answers like an agent that investigates until
    /// it is told to wrap up: a `read_file` of `read_path` while `read_file` is offered, else
    /// `submit_findings` with `findings`. The first such call takes `first_delay`, later ones
    /// `delay`. Takes precedence over scripted replies and `delay`.
    pub fn explore_until_wrap_up(
        &self,
        needle: &str,
        read_path: &str,
        findings: serde_json::Value,
        first_delay: Duration,
        delay: Duration,
    ) {
        self.explorers.lock().unwrap().push(Explorer {
            needle: needle.to_string(),
            read_path: read_path.to_string(),
            findings,
            first_delay,
            delay,
            calls: 0,
        });
    }

    fn explore(&self, haystack: &str, offered: &[String]) -> Option<(Reply, Duration)> {
        let mut explorers = self.explorers.lock().unwrap();
        let explorer = explorers
            .iter_mut()
            .find(|e| haystack.contains(e.needle.as_str()))?;
        explorer.calls += 1;
        let delay = if explorer.calls == 1 {
            explorer.first_delay
        } else {
            explorer.delay
        };
        let reply = if offered.iter().any(|t| t == "read_file") {
            Reply::tool(
                "read_file",
                serde_json::json!({ "path": explorer.read_path }),
            )
        } else {
            Reply::submit(explorer.findings.clone())
        };
        Some((reply, delay))
    }

    /// Most calls of `stage` that ever ran at the same time.
    pub fn max_in_flight(&self, stage: Stage) -> usize {
        self.in_flight
            .lock()
            .unwrap()
            .get(&stage)
            .map_or(0, |(_, max)| *max)
    }

    fn next_reply(&self, stage: Stage, haystack: &str) -> Reply {
        let mut script = self.script.lock().unwrap();
        let found = script.iter().position(|s| {
            s.stage == stage
                && s.needle
                    .as_ref()
                    .is_none_or(|n| haystack.contains(n.as_str()))
        });
        match found {
            Some(index) => script.remove(index).reply,
            None => match stage {
                Stage::Verifier => Reply::text(r#"{"keep": true, "rationale": "fake"}"#),
                Stage::Dedup => Reply::text("no answer"),
                Stage::Finder | Stage::Other => Reply::text("[]"),
            },
        }
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
        let stage = stage_of(&req.system);
        let user = req
            .messages
            .iter()
            .find_map(|m| match m {
                Message::User { parts } => parts.first().map(|UserPart::Text(t)| t.clone()),
                _ => None,
            })
            .unwrap_or_default();
        let tool_results = req
            .messages
            .iter()
            .flat_map(|m| match m {
                Message::Tool { results } => results.iter().map(|r| r.content.clone()).collect(),
                _ => Vec::new(),
            })
            .collect();
        let haystack = format!("{}\n{user}", req.system);
        let offered: Vec<String> = req.tools.iter().map(|t| t.name.clone()).collect();
        let explored = match stage {
            Stage::Finder => self.explore(&haystack, &offered),
            _ => None,
        };
        let delay = match stage {
            Stage::Finder => self
                .delays
                .lock()
                .unwrap()
                .iter()
                .find(|(needle, _)| haystack.contains(needle.as_str()))
                .map(|(_, delay)| *delay),
            _ => None,
        };
        self.calls.lock().unwrap().push(ChatCall {
            stage,
            provider_id,
            model: req.model,
            system: req.system,
            user,
            tools: offered,
            tool_results,
            max_tokens: req.max_tokens,
            effort: req.effort,
            ctx,
        });
        let (reply, delay) = match explored {
            Some((reply, delay)) => (reply, Some(delay)),
            None => (self.next_reply(stage, &haystack), delay),
        };
        {
            let mut in_flight = self.in_flight.lock().unwrap();
            let entry = in_flight.entry(stage).or_default();
            entry.0 += 1;
            entry.1 = entry.1.max(entry.0);
        }
        // Selalu menunggu sebentar, supaya panggilan lain sempat mulai (concurrency terlihat).
        // A call cut by a deadline is dropped while sleeping; its in-flight count then stays.
        tokio::time::sleep(delay.unwrap_or(Duration::from_millis(5))).await;
        if let Some(entry) = self.in_flight.lock().unwrap().get_mut(&stage) {
            entry.0 -= 1;
        }
        let response = |text: String, tool_calls: Vec<ToolCall>| ChatResponse {
            stop_reason: if tool_calls.is_empty() {
                StopReason::EndTurn
            } else {
                StopReason::ToolUse
            },
            text,
            tool_calls,
            input_tokens: 1,
            output_tokens: 1,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        };
        match reply {
            Reply::Text(text) => Ok(response(text, Vec::new())),
            Reply::Tools(calls) => {
                let mut next = self.next_call_id.lock().unwrap();
                let calls = calls
                    .into_iter()
                    .map(|(name, arguments)| {
                        *next += 1;
                        ToolCall {
                            id: format!("call_{next}"),
                            name,
                            arguments,
                            provider_meta: None,
                        }
                    })
                    .collect();
                Ok(response(String::new(), calls))
            }
            Reply::Err(err) => Err(err),
        }
    }
}

/// What `FakeCloner::clone_pr` does.
#[derive(Debug, Clone)]
pub enum CloneBehavior {
    /// A checkout holding these `(path, content)` files.
    Files(Vec<(String, String)>),
    Fail(WorkspaceError),
}

/// Cloner over a `TempDir` fixture. Records each spec's URL and each workspace root.
pub struct FakeCloner {
    behavior: Mutex<CloneBehavior>,
    /// `clone_pr` waits this long first (a slow fetch).
    delay: Mutex<Duration>,
    urls: Mutex<Vec<String>>,
    roots: Mutex<Vec<PathBuf>>,
}

impl Default for FakeCloner {
    fn default() -> Self {
        FakeCloner {
            behavior: Mutex::new(CloneBehavior::Files(Vec::new())),
            delay: Mutex::new(Duration::ZERO),
            urls: Mutex::new(Vec::new()),
            roots: Mutex::new(Vec::new()),
        }
    }
}

impl FakeCloner {
    pub fn set(&self, behavior: CloneBehavior) {
        *self.behavior.lock().unwrap() = behavior;
    }

    pub fn files(&self, files: &[(&str, &str)]) {
        let files = files
            .iter()
            .map(|(p, c)| (p.to_string(), c.to_string()))
            .collect();
        self.set(CloneBehavior::Files(files));
    }

    /// Every later `clone_pr` waits `delay` before it answers.
    pub fn delay(&self, delay: Duration) {
        *self.delay.lock().unwrap() = delay;
    }

    pub fn urls(&self) -> Vec<String> {
        self.urls.lock().unwrap().clone()
    }

    pub fn roots(&self) -> Vec<PathBuf> {
        self.roots.lock().unwrap().clone()
    }
}

#[async_trait]
impl RepoCloner for FakeCloner {
    async fn clone_pr(&self, spec: &CloneSpec) -> Result<RepoWorkspace, WorkspaceError> {
        self.urls.lock().unwrap().push(spec.url.clone());
        let delay = *self.delay.lock().unwrap();
        tokio::time::sleep(delay).await;
        let behavior = self.behavior.lock().unwrap().clone();
        let files = match behavior {
            CloneBehavior::Files(files) => files,
            CloneBehavior::Fail(err) => return Err(err),
        };
        let dir = tempfile::tempdir().unwrap();
        for (path, content) in files {
            let path = dir.path().join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        }
        let workspace = RepoWorkspace::from_temp_dir(dir).unwrap();
        self.roots
            .lock()
            .unwrap()
            .push(workspace.root().to_path_buf());
        Ok(workspace)
    }
}

/// A clone spec for `acme/api` PR 7 (what a real provider would return).
pub fn clone_spec() -> CloneSpec {
    CloneSpec {
        url: "https://github.com/acme/api.git".into(),
        refspec: "refs/pull/7/head".into(),
        fallback_refspec: None,
        expected_sha: "h1".into(),
        auth_header: "Basic secret".into(),
    }
}

/// Database yang sudah berisi connection GitHub, satu repo, dan model default.
pub struct Setup {
    pub engine: Engine,
    pub git: FakeGit,
    pub chat: Arc<FakeChat>,
    pub cloner: Arc<FakeCloner>,
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
        project_name: None,
        name: "api",
        default_branch: Some("main"),
        webhook_token: "t",
    };
    let repo = repositories::insert(pool, repo).await.unwrap().unwrap();
    let provider = ai::insert_provider(
        pool,
        ProviderKind::Openai,
        "openai",
        None,
        "gpt-test",
        Some("gpt-fast"),
    )
    .await
    .unwrap();
    let mut current = settings::get(pool).await.unwrap();
    current.default_provider_id = Some(provider.id);
    // The small-PR path is opt-in per test: most tests use tiny diffs and expect the full path.
    current.small_pr_max_lines = 0;
    settings::update(pool, &current).await.unwrap();

    let chat = Arc::new(FakeChat::default());
    let cloner = Arc::new(FakeCloner::default());
    Setup {
        engine: Engine {
            db: pool.clone(),
            cipher: Arc::new(cipher),
            git: Arc::new(git.clone()),
            chat: chat.clone(),
            cloner: cloner.clone(),
            review_timeout: REVIEW_TIMEOUT,
            specialists_review_timeout: SPECIALISTS_REVIEW_TIMEOUT,
        },
        git,
        chat,
        cloner,
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
        source_ref: None,
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

/// A file whose rendered diff (~25K chars) is too large to share a Finder batch with another
/// such file, so each one is reviewed by its own Finder agent. Line 2 is added.
pub fn big_file(path: &str) -> FileDiff {
    file(
        path,
        &format!("@@ -1,1 +1,2 @@\n a\n+{}\n", "x".repeat(25_000)),
    )
}

/// The `=== path (` header that starts a file's diff in a Finder prompt.
pub fn diff_header(path: &str) -> String {
    format!("=== {path} (")
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
