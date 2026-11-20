//! Test worker terhadap Redis dan Postgres asli dari docker-compose.dev.yml.
//! Hampir semua test memakai `FakeRunner` sebagai pengganti `Engine`; satu test memakai `Engine`
//! asli untuk memastikan baris `reviews` ikut ditandai gagal.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use adus::{JOB_TIMEOUT, JobRunner, handle_delivery, run_scheduler, run_worker};
use adus_core::crypto::Cipher;
use adus_core::job::{Job, Trigger};
use adus_core::types::ConnectionKind;
use adus_db::reviews::{self, NewReview, ReviewStatus};
use adus_db::{connections, repositories};
use adus_git::HttpGitProviderFactory;
use adus_llm::router::LlmRouter;
use adus_queue::{Delivery, Queue};
use adus_review::engine::{Engine, EngineError, REVIEW_TIMEOUT};
use async_trait::async_trait;
use redis::AsyncCommands;
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const SHORT_BLOCK: Duration = Duration::from_millis(100);
/// base64 dari 32 byte nol (sama dengan `.cargo/config.toml`).
const TEST_KEY: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";

/// Pengganti `Engine`: tidur `delay`, lalu kembalikan `result`. Mencatat jumlah panggilan dan
/// berapa job yang pernah berjalan bersamaan, plus jumlah `gave_up`.
struct FakeRunner {
    result: Result<(), EngineError>,
    delay: Duration,
    calls: AtomicUsize,
    gave_up: AtomicUsize,
    running: AtomicUsize,
    max_running: AtomicUsize,
}

impl FakeRunner {
    fn new(result: Result<(), EngineError>, delay: Duration) -> Arc<FakeRunner> {
        Arc::new(FakeRunner {
            result,
            delay,
            calls: AtomicUsize::new(0),
            gave_up: AtomicUsize::new(0),
            running: AtomicUsize::new(0),
            max_running: AtomicUsize::new(0),
        })
    }
}

#[async_trait]
impl JobRunner for FakeRunner {
    async fn run(&self, _queue: &Queue, _job: &Job) -> Result<(), EngineError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let now_running = self.running.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_running.fetch_max(now_running, Ordering::SeqCst);
        tokio::time::sleep(self.delay).await;
        self.running.fetch_sub(1, Ordering::SeqCst);
        self.result.clone()
    }

    async fn gave_up(&self, _job: &Job, _error: &str) {
        self.gave_up.fetch_add(1, Ordering::SeqCst);
    }
}

/// Job yang selalu panic.
struct PanickingRunner;

#[async_trait]
impl JobRunner for PanickingRunner {
    async fn run(&self, _queue: &Queue, _job: &Job) -> Result<(), EngineError> {
        panic!("bug in job");
    }
}

fn redis_url() -> String {
    std::env::var("REDIS_URL").expect("REDIS_URL must be set (see .cargo/config.toml)")
}

async fn new_queue() -> (Queue, String) {
    let prefix = format!("test-{}", Uuid::new_v4());
    let queue = Queue::connect(&redis_url(), &prefix).await.unwrap();
    (queue, prefix)
}

async fn raw_connection() -> redis::aio::MultiplexedConnection {
    let client = redis::Client::open(redis_url()).unwrap();
    client.get_multiplexed_async_connection().await.unwrap()
}

async fn cleanup(prefix: &str) {
    let mut conn = raw_connection().await;
    let keys: Vec<String> = conn.keys(format!("{prefix}:*")).await.unwrap();
    if !keys.is_empty() {
        let _: () = conn.del(keys).await.unwrap();
    }
}

fn review_job(repository_id: Uuid, pr_number: i64) -> Job {
    Job::ReviewPr {
        repository_id,
        pr_number,
        trigger: Trigger::Opened,
    }
}

/// Masukkan `job` ke stream sebagai percobaan `attempt`, lalu ambil delivery-nya.
async fn deliver(queue: &Queue, job: &Job, attempt: u32) -> Delivery {
    queue
        .enqueue_delayed(job, attempt, Duration::ZERO)
        .await
        .unwrap();
    assert_eq!(queue.promote_due().await.unwrap(), 1);
    queue
        .next("test-worker", SHORT_BLOCK)
        .await
        .unwrap()
        .unwrap()
}

fn now_ms() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as f64
}

/// Isi `{prefix}:delayed` sebagai (attempt, detik dari sekarang sampai jatuh tempo).
async fn delayed(prefix: &str) -> Vec<(u32, f64)> {
    let mut conn = raw_connection().await;
    let members: Vec<(String, f64)> = conn
        .zrange_withscores(format!("{prefix}:delayed"), 0, -1)
        .await
        .unwrap();
    members
        .into_iter()
        .map(|(member, score)| {
            let entry: serde_json::Value = serde_json::from_str(&member).unwrap();
            let attempt = entry["attempt"].as_u64().unwrap() as u32;
            (attempt, (score - now_ms()) / 1000.0)
        })
        .collect()
}

async fn stream_len(key: &str) -> usize {
    let mut conn = raw_connection().await;
    conn.xlen(key).await.unwrap()
}

/// Isi `{prefix}:jobs:dead` sebagai (attempt, error).
async fn dead_letters(prefix: &str) -> Vec<(String, String)> {
    let mut conn = raw_connection().await;
    let entries: Vec<(String, std::collections::HashMap<String, String>)> = redis::cmd("XRANGE")
        .arg(format!("{prefix}:jobs:dead"))
        .arg("-")
        .arg("+")
        .query_async(&mut conn)
        .await
        .unwrap();
    entries
        .into_iter()
        .map(|(_, fields)| (fields["attempt"].clone(), fields["error"].clone()))
        .collect()
}

async fn lock_exists(prefix: &str, repository_id: Uuid, pr_number: i64) -> bool {
    let mut conn = raw_connection().await;
    conn.exists(format!("{prefix}:lock:pr:{repository_id}:{pr_number}"))
        .await
        .unwrap()
}

#[tokio::test]
async fn retryable_error_requeued_with_backoff() {
    // (attempt sekarang, attempt berikutnya, backoff dalam detik)
    for (attempt, next_attempt, backoff_secs) in [(1, 2, 10.0), (4, 5, 80.0)] {
        let (queue, prefix) = new_queue().await;
        let repository_id = Uuid::new_v4();
        let delivery = deliver(&queue, &review_job(repository_id, 1), attempt).await;
        let runner = FakeRunner::new(
            Err(EngineError::Retryable("git provider: 502".into(), None)),
            Duration::ZERO,
        );

        handle_delivery(&queue, runner.clone(), delivery, JOB_TIMEOUT)
            .await
            .unwrap();

        let retries = delayed(&prefix).await;
        assert_eq!(retries.len(), 1);
        assert_eq!(runner.gave_up.load(Ordering::SeqCst), 0);
        let (stored_attempt, due_in) = retries[0];
        assert_eq!(stored_attempt, next_attempt);
        let expected = (backoff_secs - 1.0)..=(backoff_secs + 0.5);
        assert!(expected.contains(&due_in), "due in {due_in}s");
        assert_eq!(stream_len(&format!("{prefix}:jobs")).await, 0);
        assert!(!lock_exists(&prefix, repository_id, 1).await);
        cleanup(&prefix).await;
    }
}

/// Koneksi GitHub `acme` dengan satu repo `acme/api`.
async fn insert_repo(pool: &PgPool, cipher: &Cipher) -> repositories::Repository {
    let conn = connections::insert(
        pool,
        ConnectionKind::Github,
        "gh",
        "acme",
        &cipher.encrypt("ghp_test"),
        "test",
        "adudu",
    )
    .await
    .unwrap();
    let new_repo = repositories::NewRepository {
        connection_id: conn.id,
        external_id: "42",
        owner: "acme",
        project: None,
        project_name: None,
        name: "api",
        default_branch: Some("main"),
        webhook_token: "t",
    };
    repositories::insert(pool, new_repo).await.unwrap().unwrap()
}

#[sqlx::test(migrations = "../../migrations")]
async fn fifth_failure_goes_to_dead_letter_and_marks_review_failed(pool: PgPool) {
    let (queue, prefix) = new_queue().await;
    let cipher = Arc::new(Cipher::from_base64_key(TEST_KEY).unwrap());
    let repo = insert_repo(&pool, &cipher).await;
    // Port 1 menolak koneksi: GitHub "sedang down", jadi engine mengembalikan Retryable.
    let http = reqwest::Client::new();
    let git = HttpGitProviderFactory {
        http: http.clone(),
        github_base: "http://127.0.0.1:1".into(),
        azure_base: "http://127.0.0.1:1".into(),
    };
    let engine = Engine {
        db: pool.clone(),
        cipher: cipher.clone(),
        git: Arc::new(git),
        chat: Arc::new(LlmRouter::new(pool.clone(), cipher, http)),
        review_timeout: REVIEW_TIMEOUT,
    };
    let delivery = deliver(&queue, &review_job(repo.id, 7), 5).await;

    handle_delivery(&queue, Arc::new(engine), delivery, JOB_TIMEOUT)
        .await
        .unwrap();

    let dead = dead_letters(&prefix).await;
    assert_eq!(dead.len(), 1);
    assert_eq!(dead[0].0, "5");
    assert!(dead[0].1.starts_with("git provider:"), "{}", dead[0].1);
    assert!(delayed(&prefix).await.is_empty());
    // Engine menghapus barisnya sendiri (Retryable); worker mencatat tepat satu baris gagal.
    let rows: Vec<(ReviewStatus, Option<String>)> =
        sqlx::query_as("SELECT status, error FROM reviews WHERE repository_id = $1")
            .bind(repo.id)
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, ReviewStatus::Failed);
    let error = rows[0].1.as_deref().unwrap();
    assert!(error.starts_with("git provider:"), "{error}");
    assert!(error.ends_with("(gave up after 5 attempts)"), "{error}");
    cleanup(&prefix).await;
}

#[tokio::test]
async fn rate_limit_retry_uses_retry_after_when_longer() {
    let (queue, prefix) = new_queue().await;
    let delivery = deliver(&queue, &review_job(Uuid::new_v4(), 1), 1).await;
    let runner = FakeRunner::new(
        Err(EngineError::Retryable(
            "LLM: all keys failed".into(),
            Some(Duration::from_secs(600)),
        )),
        Duration::ZERO,
    );

    handle_delivery(&queue, runner.clone(), delivery, JOB_TIMEOUT)
        .await
        .unwrap();

    let retries = delayed(&prefix).await;
    assert_eq!(retries.len(), 1);
    let (attempt, due_in) = retries[0];
    assert_eq!(attempt, 2);
    assert!((599.0..=600.5).contains(&due_in), "due in {due_in}s");
    cleanup(&prefix).await;
}

#[tokio::test]
async fn same_pr_jobs_never_run_concurrently() {
    let (queue, prefix) = new_queue().await;
    let repository_id = Uuid::new_v4();
    let runner = FakeRunner::new(Ok(()), Duration::from_millis(500));
    let shutdown = CancellationToken::new();
    let workers: Vec<_> = (0..2)
        .map(|index| {
            tokio::spawn(run_worker(
                format!("test-worker-{index}"),
                queue.clone(),
                runner.clone(),
                JOB_TIMEOUT,
                shutdown.clone(),
            ))
        })
        .collect();

    queue.enqueue(&review_job(repository_id, 3)).await.unwrap();
    queue.enqueue(&review_job(repository_id, 3)).await.unwrap();

    // Satu job jalan; yang lain mendapati lock terpegang dan dijadwalkan ulang.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while runner.calls.load(Ordering::SeqCst) < 1 || delayed(&prefix).await.is_empty() {
        assert!(tokio::time::Instant::now() < deadline, "timed out");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let live = queue.live_workers().await.unwrap();
    assert_eq!(
        live,
        vec!["test-worker-0".to_string(), "test-worker-1".to_string()]
    );
    shutdown.cancel();
    for worker in workers {
        worker.await.unwrap();
    }

    assert_eq!(runner.calls.load(Ordering::SeqCst), 1);
    assert_eq!(runner.max_running.load(Ordering::SeqCst), 1);
    assert_eq!(delayed(&prefix).await.len(), 1);
    assert_eq!(stream_len(&format!("{prefix}:jobs")).await, 0);
    cleanup(&prefix).await;
}

#[tokio::test]
async fn lock_busy_requeues_without_consuming_attempt() {
    let (queue, prefix) = new_queue().await;
    let repository_id = Uuid::new_v4();
    let held = queue
        .try_lock(&format!("pr:{repository_id}:4"), Duration::from_secs(60))
        .await
        .unwrap();
    assert!(held.is_some());
    let comment = Job::PrComment {
        repository_id,
        pr_number: 4,
        comment_url: None,
        author: adus_core::job::CommentAuthor {
            login: Some("octocat".into()),
            email: None,
        },
        body: "@adudu review".into(),
    };
    let delivery = deliver(&queue, &comment, 3).await;
    let runner = FakeRunner::new(Ok(()), Duration::ZERO);

    handle_delivery(&queue, runner.clone(), delivery, JOB_TIMEOUT)
        .await
        .unwrap();

    assert_eq!(runner.calls.load(Ordering::SeqCst), 0);
    let retries = delayed(&prefix).await;
    assert_eq!(retries.len(), 1);
    let (attempt, due_in) = retries[0];
    assert_eq!(attempt, 3);
    assert!((29.0..=30.5).contains(&due_in), "due in {due_in}s");
    assert_eq!(stream_len(&format!("{prefix}:jobs")).await, 0);
    // Lock milik pihak lain tidak disentuh.
    assert!(lock_exists(&prefix, repository_id, 4).await);
    cleanup(&prefix).await;
}

#[tokio::test]
async fn fatal_goes_straight_to_dead_letter() {
    let (queue, prefix) = new_queue().await;
    let repository_id = Uuid::new_v4();
    let delivery = deliver(&queue, &review_job(repository_id, 1), 1).await;
    let runner = FakeRunner::new(
        Err(EngineError::Fatal("pull request not found".into())),
        Duration::ZERO,
    );

    handle_delivery(&queue, runner.clone(), delivery, JOB_TIMEOUT)
        .await
        .unwrap();

    let dead = dead_letters(&prefix).await;
    assert_eq!(
        dead,
        vec![("1".to_string(), "pull request not found".to_string())]
    );
    // Fatal: engine sudah mencatat barisnya sendiri.
    assert_eq!(runner.gave_up.load(Ordering::SeqCst), 0);
    assert!(delayed(&prefix).await.is_empty());
    assert!(!lock_exists(&prefix, repository_id, 1).await);
    cleanup(&prefix).await;
}

#[tokio::test]
async fn job_timeout_is_retryable() {
    let (queue, prefix) = new_queue().await;
    let repository_id = Uuid::new_v4();
    let delivery = deliver(&queue, &review_job(repository_id, 1), 1).await;
    let runner = FakeRunner::new(Ok(()), Duration::from_secs(5));

    handle_delivery(&queue, runner.clone(), delivery, Duration::from_millis(100))
        .await
        .unwrap();

    let retries = delayed(&prefix).await;
    assert_eq!(retries.len(), 1);
    assert_eq!(retries[0].0, 2);
    assert!(!lock_exists(&prefix, repository_id, 1).await);
    cleanup(&prefix).await;
}

#[tokio::test]
async fn heartbeat_visible_then_expires() {
    let (queue, prefix) = new_queue().await;

    queue.heartbeat("worker-a").await.unwrap();
    queue.heartbeat("worker-b").await.unwrap();

    let mut live = queue.live_workers().await.unwrap();
    live.sort();
    assert_eq!(live, vec!["worker-a".to_string(), "worker-b".to_string()]);
    let key = format!("{prefix}:worker:worker-a:hb");
    let mut conn = raw_connection().await;
    let ttl: i64 = conn.ttl(&key).await.unwrap();
    assert!((1..=30).contains(&ttl), "ttl {ttl}");

    // Mempercepat kedaluwarsa (EX 30 terlalu lama untuk test).
    let _: () = conn.pexpire(&key, 1).await.unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(
        queue.live_workers().await.unwrap(),
        vec!["worker-b".to_string()]
    );
    cleanup(&prefix).await;
}

#[tokio::test]
async fn panicking_job_does_not_kill_worker() {
    let (queue, prefix) = new_queue().await;
    let shutdown = CancellationToken::new();
    let worker = tokio::spawn(run_worker(
        "test-worker".to_string(),
        queue.clone(),
        Arc::new(PanickingRunner),
        JOB_TIMEOUT,
        shutdown.clone(),
    ));
    let first = Uuid::new_v4();
    let second = Uuid::new_v4();

    // Job kedua hanya dikerjakan jika worker selamat dari panic job pertama.
    queue.enqueue(&review_job(first, 1)).await.unwrap();
    queue.enqueue(&review_job(second, 1)).await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while delayed(&prefix).await.len() < 2 {
        assert!(tokio::time::Instant::now() < deadline, "timed out");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    shutdown.cancel();
    worker.await.unwrap();

    let attempts: Vec<u32> = delayed(&prefix).await.iter().map(|d| d.0).collect();
    assert_eq!(attempts, vec![2, 2]);
    assert!(!lock_exists(&prefix, first, 1).await);
    assert!(!lock_exists(&prefix, second, 1).await);
    cleanup(&prefix).await;
}

#[sqlx::test(migrations = "../../migrations")]
async fn scheduler_sweeps_stale_reviews(pool: PgPool) {
    let (queue, prefix) = new_queue().await;
    let cipher = Cipher::from_base64_key(TEST_KEY).unwrap();
    let repo = insert_repo(&pool, &cipher).await;
    let new = |pr_number| NewReview {
        repository_id: repo.id,
        pr_number,
        trigger: Trigger::Opened,
    };
    let stale = reviews::insert(&pool, new(1)).await.unwrap().id;
    let fresh = reviews::insert(&pool, new(2)).await.unwrap().id;
    sqlx::query("UPDATE reviews SET created_at = now() - interval '20 minutes' WHERE id = $1")
        .bind(stale)
        .execute(&pool)
        .await
        .unwrap();
    let status = |id: Uuid| {
        let pool = pool.clone();
        async move {
            let row: (ReviewStatus,) = sqlx::query_as("SELECT status FROM reviews WHERE id = $1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap();
            row.0
        }
    };

    let shutdown = CancellationToken::new();
    let scheduler = tokio::spawn(run_scheduler(
        queue,
        pool.clone(),
        Duration::from_millis(50),
        shutdown.clone(),
    ));
    let mut swept = false;
    for _ in 0..100 {
        if status(stale).await == ReviewStatus::Failed {
            swept = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    shutdown.cancel();
    scheduler.await.unwrap();

    assert!(
        swept,
        "the stale review is marked failed while the scheduler runs"
    );
    assert_eq!(status(fresh).await, ReviewStatus::Queued);
    cleanup(&prefix).await;
}
