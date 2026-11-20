//! Test antrian terhadap Redis asli dari docker-compose.dev.yml (REDIS_URL di .cargo/config.toml).
//! Tiap test memakai prefix unik `test-{uuid}` dan menghapus key-nya di akhir.

use std::collections::HashMap;
use std::time::Duration;

use adus_core::job::{CommentAuthor, Job, Trigger};
use adus_queue::{MAX_ATTEMPTS, Queue, backoff};
use redis::AsyncCommands;
use uuid::Uuid;

const SHORT_BLOCK: Duration = Duration::from_millis(100);

fn redis_url() -> String {
    std::env::var("REDIS_URL").expect("REDIS_URL must be set (see .cargo/config.toml)")
}

fn review_job(pr_number: i64) -> Job {
    Job::ReviewPr {
        repository_id: Uuid::new_v4(),
        pr_number,
        trigger: Trigger::Opened,
    }
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

/// Isi stream sebagai daftar (id, field -> nilai).
async fn stream_entries(key: &str) -> Vec<(String, HashMap<String, String>)> {
    let mut conn = raw_connection().await;
    let entries: Vec<(String, HashMap<String, String>)> = redis::cmd("XRANGE")
        .arg(key)
        .arg("-")
        .arg("+")
        .query_async(&mut conn)
        .await
        .unwrap();
    entries
}

async fn cleanup(prefix: &str) {
    let mut conn = raw_connection().await;
    let keys: Vec<String> = conn.keys(format!("{prefix}:*")).await.unwrap();
    if !keys.is_empty() {
        let _: () = conn.del(keys).await.unwrap();
    }
}

#[tokio::test]
async fn enqueue_then_next_then_ack() {
    let (queue, prefix) = new_queue().await;
    let job = Job::PrComment {
        repository_id: Uuid::new_v4(),
        pr_number: 3,
        comment_url: None,
        author: CommentAuthor {
            login: Some("octocat".to_string()),
            email: Some("octo@example.test".to_string()),
        },
        body: "@adudu review".to_string(),
    };

    queue.enqueue(&job).await.unwrap();
    let delivery = queue.next("worker-1", SHORT_BLOCK).await.unwrap().unwrap();
    assert_eq!(delivery.job, job);
    assert_eq!(delivery.attempt, 1);

    // Belum di-ack: konsumen lain tidak menerimanya lagi (belum idle 15 menit).
    assert!(queue.next("worker-2", SHORT_BLOCK).await.unwrap().is_none());

    queue.ack(&delivery).await.unwrap();
    assert!(stream_entries(&format!("{prefix}:jobs")).await.is_empty());
    let mut conn = raw_connection().await;
    let pending: (u64, Option<String>, Option<String>, redis::Value) = redis::cmd("XPENDING")
        .arg(format!("{prefix}:jobs"))
        .arg("adus-workers")
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(pending.0, 0);
    assert!(queue.next("worker-1", SHORT_BLOCK).await.unwrap().is_none());

    cleanup(&prefix).await;
}

#[tokio::test]
async fn connect_twice_ignores_existing_group() {
    let (_queue, prefix) = new_queue().await;
    Queue::connect(&redis_url(), &prefix).await.unwrap();
    cleanup(&prefix).await;
}

#[tokio::test]
async fn first_seen_true_once() {
    let (queue, prefix) = new_queue().await;
    assert!(queue.first_seen("github:delivery-1").await.unwrap());
    assert!(!queue.first_seen("github:delivery-1").await.unwrap());
    assert!(queue.first_seen("github:delivery-2").await.unwrap());

    let mut conn = raw_connection().await;
    let ttl: i64 = conn
        .ttl(format!("{prefix}:dedup:github:delivery-1"))
        .await
        .unwrap();
    assert!(ttl > 86_000 && ttl <= 86_400, "ttl was {ttl}");

    cleanup(&prefix).await;
}

#[tokio::test]
async fn enqueue_once_adds_one_entry_per_key() {
    let (queue, prefix) = new_queue().await;
    assert!(queue.enqueue_once(&review_job(1), "gh:d-1").await.unwrap());
    assert!(!queue.enqueue_once(&review_job(1), "gh:d-1").await.unwrap());
    assert!(queue.enqueue_once(&review_job(2), "gh:d-2").await.unwrap());

    let entries = stream_entries(&format!("{prefix}:jobs")).await;
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].1["attempt"], "1");
    let delivery = queue.next("worker-1", SHORT_BLOCK).await.unwrap().unwrap();
    assert!(matches!(delivery.job, Job::ReviewPr { pr_number: 1, .. }));
    assert_eq!(delivery.attempt, 1);

    let mut conn = raw_connection().await;
    let ttl: i64 = conn.ttl(format!("{prefix}:dedup:gh:d-1")).await.unwrap();
    assert!(ttl > 86_000 && ttl <= 86_400, "ttl was {ttl}");

    cleanup(&prefix).await;
}

#[tokio::test]
async fn delayed_job_not_visible_before_due() {
    let (queue, prefix) = new_queue().await;
    queue
        .enqueue_delayed(&review_job(1), 2, Duration::from_secs(60))
        .await
        .unwrap();

    assert_eq!(queue.promote_due().await.unwrap(), 0);
    assert!(queue.next("worker-1", SHORT_BLOCK).await.unwrap().is_none());

    cleanup(&prefix).await;
}

#[tokio::test]
async fn promote_due_moves_job_with_attempt() {
    let (queue, prefix) = new_queue().await;
    // 2^53 + 1: tidak bisa diwakili f64, jadi membuktikan JSON job tidak diubah oleh Lua.
    let job = review_job(9_007_199_254_740_993);
    queue
        .enqueue_delayed(&job, 3, Duration::ZERO)
        .await
        .unwrap();
    // Job identik dengan delay sama tetap dua entri terpisah.
    queue
        .enqueue_delayed(&job, 3, Duration::ZERO)
        .await
        .unwrap();

    assert_eq!(queue.promote_due().await.unwrap(), 2);
    assert_eq!(queue.promote_due().await.unwrap(), 0);

    for _ in 0..2 {
        let delivery = queue.next("worker-1", SHORT_BLOCK).await.unwrap().unwrap();
        assert_eq!(delivery.job, job);
        assert_eq!(delivery.attempt, 3);
        queue.ack(&delivery).await.unwrap();
    }
    assert!(queue.next("worker-1", SHORT_BLOCK).await.unwrap().is_none());

    cleanup(&prefix).await;
}

#[tokio::test]
async fn concurrent_promote_due_moves_each_job_once() {
    let (queue_a, prefix) = new_queue().await;
    let queue_b = Queue::connect(&redis_url(), &prefix).await.unwrap();
    let mut expected = Vec::new();
    for pr_number in 0..50 {
        let job = review_job(pr_number);
        queue_a
            .enqueue_delayed(&job, 1, Duration::ZERO)
            .await
            .unwrap();
        expected.push(serde_json::to_string(&job).unwrap());
    }

    let (moved_a, moved_b) = tokio::join!(queue_a.promote_due(), queue_b.promote_due());
    assert_eq!(moved_a.unwrap() + moved_b.unwrap(), 50);

    let mut in_stream: Vec<String> = stream_entries(&format!("{prefix}:jobs"))
        .await
        .into_iter()
        .map(|(_, fields)| fields["job"].clone())
        .collect();
    in_stream.sort();
    expected.sort();
    assert_eq!(in_stream, expected);

    cleanup(&prefix).await;
}

#[tokio::test]
async fn dead_letter_lands_in_dead_stream() {
    let (queue, prefix) = new_queue().await;
    let job = review_job(5);
    queue.enqueue(&job).await.unwrap();
    let delivery = queue.next("worker-1", SHORT_BLOCK).await.unwrap().unwrap();

    queue.dead_letter(&delivery, "boom").await.unwrap();

    let dead = stream_entries(&format!("{prefix}:jobs:dead")).await;
    assert_eq!(dead.len(), 1);
    let fields = &dead[0].1;
    assert_eq!(fields["job"], serde_json::to_string(&job).unwrap());
    assert_eq!(fields["attempt"], "1");
    assert_eq!(fields["error"], "boom");
    assert!(chrono::DateTime::parse_from_rfc3339(&fields["failed_at"]).is_ok());
    // Job asli sudah di-ack dan dihapus.
    assert!(stream_entries(&format!("{prefix}:jobs")).await.is_empty());
    assert!(queue.next("worker-1", SHORT_BLOCK).await.unwrap().is_none());

    cleanup(&prefix).await;
}

#[tokio::test]
async fn malformed_entry_is_dead_lettered_and_queue_continues() {
    let (queue, prefix) = new_queue().await;
    let mut conn = raw_connection().await;
    let _: String = conn
        .xadd(
            format!("{prefix}:jobs"),
            "*",
            &[("job", "{not json"), ("attempt", "2")],
        )
        .await
        .unwrap();
    let job = review_job(6);
    queue.enqueue(&job).await.unwrap();

    let delivery = queue.next("worker-1", SHORT_BLOCK).await.unwrap().unwrap();
    assert_eq!(delivery.job, job);

    let dead = stream_entries(&format!("{prefix}:jobs:dead")).await;
    assert_eq!(dead.len(), 1);
    assert_eq!(dead[0].1["job"], "{not json");
    assert_eq!(dead[0].1["attempt"], "2");
    assert!(!dead[0].1["error"].is_empty());
    // Hanya job yang valid yang tersisa di stream.
    assert_eq!(stream_entries(&format!("{prefix}:jobs")).await.len(), 1);

    cleanup(&prefix).await;
}

/// Pura-pura pemilik entri mati 16 menit lalu: XCLAIM dengan IDLE mengatur waktu idle entri.
/// JUSTID membuat XCLAIM tidak menaikkan hitungan delivery; RETRYCOUNT menimpanya.
async fn make_stale(prefix: &str, entry_id: &str, retry_count: Option<u32>) {
    let mut conn = raw_connection().await;
    let mut cmd = redis::cmd("XCLAIM");
    cmd.arg(format!("{prefix}:jobs"))
        .arg("adus-workers")
        .arg("worker-1")
        .arg(0)
        .arg(entry_id)
        .arg("IDLE")
        .arg(16 * 60 * 1000);
    if let Some(count) = retry_count {
        cmd.arg("RETRYCOUNT").arg(count);
    }
    cmd.arg("JUSTID");
    let _: redis::Value = cmd.query_async(&mut conn).await.unwrap();
}

#[tokio::test]
async fn stale_delivery_is_reclaimed_by_another_consumer() {
    let (queue, prefix) = new_queue().await;
    let job = review_job(8);
    queue.enqueue(&job).await.unwrap();
    let first = queue.next("worker-1", SHORT_BLOCK).await.unwrap().unwrap();
    assert_eq!(first.attempt, 1);

    make_stale(&prefix, &first.id, None).await;

    // Worker-1 mati di tengah percobaan 1, jadi pengambilalihan dihitung percobaan 2.
    let second = queue.next("worker-2", SHORT_BLOCK).await.unwrap().unwrap();
    assert_eq!(second.id, first.id);
    assert_eq!(second.job, job);
    assert_eq!(second.attempt, 2);

    cleanup(&prefix).await;
}

#[tokio::test]
async fn job_that_keeps_killing_workers_is_dead_lettered() {
    let (queue, prefix) = new_queue().await;
    let job = review_job(9);
    queue.enqueue(&job).await.unwrap();
    let first = queue.next("worker-1", SHORT_BLOCK).await.unwrap().unwrap();

    // Sudah dikirim 5 kali; pengambilalihan berikutnya akan menjadi percobaan 6.
    make_stale(&prefix, &first.id, Some(5)).await;

    assert!(queue.next("worker-2", SHORT_BLOCK).await.unwrap().is_none());
    let dead = stream_entries(&format!("{prefix}:jobs:dead")).await;
    assert_eq!(dead.len(), 1);
    assert_eq!(dead[0].1["job"], serde_json::to_string(&job).unwrap());
    assert_eq!(dead[0].1["attempt"], "6");
    assert_eq!(dead[0].1["error"], "exceeded max deliveries (worker died)");
    assert!(stream_entries(&format!("{prefix}:jobs")).await.is_empty());

    cleanup(&prefix).await;
}

#[tokio::test]
async fn retry_moves_delivery_to_delayed_set_atomically() {
    let (queue, prefix) = new_queue().await;
    let job = review_job(10);
    queue.enqueue(&job).await.unwrap();
    let delivery = queue.next("worker-1", SHORT_BLOCK).await.unwrap().unwrap();

    queue.retry(&delivery, 2, Duration::ZERO).await.unwrap();

    // Sudah di-ack dan dihapus dari stream, dan menunggu di delayed set.
    assert!(stream_entries(&format!("{prefix}:jobs")).await.is_empty());
    let mut conn = raw_connection().await;
    let delayed: u64 = conn.zcard(format!("{prefix}:delayed")).await.unwrap();
    assert_eq!(delayed, 1);
    assert!(queue.next("worker-1", SHORT_BLOCK).await.unwrap().is_none());

    assert_eq!(queue.promote_due().await.unwrap(), 1);
    let again = queue.next("worker-1", SHORT_BLOCK).await.unwrap().unwrap();
    assert_eq!(again.job, job);
    assert_eq!(again.attempt, 2);
    assert_ne!(again.id, delivery.id);

    cleanup(&prefix).await;
}

#[tokio::test]
async fn lock_is_exclusive_and_unlock_requires_token() {
    let (queue, prefix) = new_queue().await;
    let ttl = Duration::from_secs(30);

    let token = queue.try_lock("repo-1:pr-2", ttl).await.unwrap().unwrap();
    assert!(queue.try_lock("repo-1:pr-2", ttl).await.unwrap().is_none());

    queue.unlock("repo-1:pr-2", "wrong-token").await.unwrap();
    assert!(queue.try_lock("repo-1:pr-2", ttl).await.unwrap().is_none());

    queue.unlock("repo-1:pr-2", &token).await.unwrap();
    let again = queue.try_lock("repo-1:pr-2", ttl).await.unwrap().unwrap();
    assert_ne!(again, token);

    let mut conn = raw_connection().await;
    let pttl: i64 = conn
        .pttl(format!("{prefix}:lock:repo-1:pr-2"))
        .await
        .unwrap();
    assert!(pttl > 0 && pttl <= 30_000, "pttl was {pttl}");

    cleanup(&prefix).await;
}

#[test]
fn backoff_values() {
    assert_eq!(backoff(1), Duration::from_secs(10));
    assert_eq!(backoff(2), Duration::from_secs(20));
    assert_eq!(backoff(5), Duration::from_secs(160));
    // Di luar 1..=5 dijepit.
    assert_eq!(backoff(0), Duration::from_secs(10));
    assert_eq!(backoff(9), Duration::from_secs(160));
    assert_eq!(backoff(MAX_ATTEMPTS), Duration::from_secs(160));
}
