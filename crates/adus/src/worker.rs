//! Worker: ambil job dari antrian, jalankan lewat `JobRunner` (engine review), lalu ack,
//! jadwalkan ulang, atau pindahkan ke stream dead. Plus scheduler yang memindahkan job
//! delayed yang jatuh tempo ke stream.

use std::sync::Arc;
use std::time::Duration;

use adus_core::job::Job;
use adus_db::reviews::{self, NewReview};
use adus_queue::{Delivery, MAX_ATTEMPTS, Queue, QueueError, backoff};
use adus_review::engine::{Engine, EngineError, REVIEW_TIMEOUT};
use async_trait::async_trait;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

/// Batas waktu cadangan satu job. Review sudah dibatasi 12 menit di dalam engine (supaya baris
/// `reviews` tercatat gagal); ini menjaga sisanya. Di bawah TTL lock dan batas ambil-alih antrian
/// (keduanya 15 menit), jadi job yang macet tidak pernah dikerjakan dua worker sekaligus.
pub const JOB_TIMEOUT: Duration = Duration::from_secs(13 * 60);
const LOCK_TTL: Duration = Duration::from_secs(15 * 60);
const _: () = assert!(REVIEW_TIMEOUT.as_secs() < JOB_TIMEOUT.as_secs());
const _: () = assert!(JOB_TIMEOUT.as_secs() < LOCK_TTL.as_secs());
/// Jeda sebelum mencoba lagi job yang PR-nya sedang dikerjakan worker lain.
const LOCK_BUSY_DELAY: Duration = Duration::from_secs(30);
const NEXT_BLOCK: Duration = Duration::from_secs(5);
const HEARTBEAT_EVERY: Duration = Duration::from_secs(10);
const PROMOTE_EVERY: Duration = Duration::from_secs(1);
/// Jeda setelah error Redis (koneksi putus sesaat) sebelum mencoba lagi.
const ERROR_PAUSE: Duration = Duration::from_secs(1);

/// Yang menjalankan job. Implementasi asli: `Engine`; test memakai fake.
#[async_trait]
pub trait JobRunner: Send + Sync {
    async fn run(&self, queue: &Queue, job: &Job) -> Result<(), EngineError>;

    /// Dipanggil sekali setelah job menyerah karena semua percobaannya gagal sementara.
    /// `error` sudah memuat jumlah percobaan. Bawaan: tidak melakukan apa pun.
    async fn gave_up(&self, _job: &Job, _error: &str) {}
}

#[async_trait]
impl JobRunner for Engine {
    async fn run(&self, queue: &Queue, job: &Job) -> Result<(), EngineError> {
        match job {
            Job::ReviewPr {
                repository_id,
                pr_number,
                trigger,
            } => self
                .review_pr(*repository_id, *pr_number, *trigger)
                .await
                .map(|_review_id| ()),
            Job::PrComment { .. } => self.handle_comment(queue, job).await,
        }
    }

    /// Engine menghapus barisnya sendiri pada error sementara, jadi review yang menyerah
    /// dicatat di sini sebagai satu baris `failed`.
    async fn gave_up(&self, job: &Job, error: &str) {
        let Job::ReviewPr {
            repository_id,
            pr_number,
            trigger,
        } = job
        else {
            return;
        };
        let new = NewReview {
            repository_id: *repository_id,
            pr_number: *pr_number,
            trigger: *trigger,
        };
        if let Err(err) = reviews::insert_failed(&self.db, new, error).await {
            tracing::warn!(error = %err, "could not record the failed review");
        }
    }
}

/// Loop satu worker sampai `shutdown` dibatalkan. Job yang sedang jalan diselesaikan dulu.
pub async fn run_worker(
    name: String,
    queue: Queue,
    runner: Arc<dyn JobRunner>,
    job_timeout: Duration,
    shutdown: CancellationToken,
) {
    // Heartbeat di task terpisah, supaya tetap jalan selama job panjang.
    let heartbeat = tokio::spawn(send_heartbeats(
        name.clone(),
        queue.clone(),
        shutdown.clone(),
    ));
    tracing::info!(worker = %name, "worker started");
    loop {
        // Membatalkan `next` di tengah jalan aman: entri yang sudah terkirim tetap pending
        // dan diambil alih nanti (lihat `Queue::next`).
        let next = tokio::select! {
            _ = shutdown.cancelled() => break,
            next = queue.next(&name, NEXT_BLOCK) => next,
        };
        let result = match next {
            Ok(Some(delivery)) => {
                handle_delivery(&queue, runner.clone(), delivery, job_timeout).await
            }
            Ok(None) => Ok(()),
            Err(err) => Err(err),
        };
        if let Err(err) = result {
            tracing::warn!(worker = %name, error = %err, "queue error; retrying in 1 s");
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = tokio::time::sleep(ERROR_PAUSE) => {}
            }
        }
    }
    let _ = heartbeat.await;
    tracing::info!(worker = %name, "worker stopped");
}

/// Pindahkan job delayed yang jatuh tempo ke stream tiap detik, sampai `shutdown` dibatalkan.
pub async fn run_scheduler(queue: Queue, shutdown: CancellationToken) {
    let mut interval = tokio::time::interval(PROMOTE_EVERY);
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            _ = interval.tick() => {}
        }
        if let Err(err) = queue.promote_due().await {
            tracing::warn!(error = %err, "could not promote delayed jobs");
        }
    }
}

/// Kerjakan satu delivery di bawah lock per PR, lalu selesaikan di antrian.
/// Error yang dikembalikan hanya error antrian; error job sudah ditangani di sini.
pub async fn handle_delivery(
    queue: &Queue,
    runner: Arc<dyn JobRunner>,
    delivery: Delivery,
    job_timeout: Duration,
) -> Result<(), QueueError> {
    // Isi job (teks komentar) sengaja tidak di-log.
    let (kind, repository_id, pr_number) = match &delivery.job {
        Job::ReviewPr {
            repository_id,
            pr_number,
            ..
        } => ("review_pr", repository_id, pr_number),
        Job::PrComment {
            repository_id,
            pr_number,
            ..
        } => ("pr_comment", repository_id, pr_number),
    };
    let span =
        tracing::info_span!("job", kind, %repository_id, pr_number, attempt = delivery.attempt);
    let lock_key = format!("pr:{repository_id}:{pr_number}");

    async {
        let Some(token) = queue.try_lock(&lock_key, LOCK_TTL).await? else {
            // PR sedang dikerjakan worker lain: coba lagi nanti tanpa menambah attempt.
            tracing::info!(outcome = "pr_busy", "pull request busy; retrying in 30 s");
            return queue
                .retry(&delivery, delivery.attempt, LOCK_BUSY_DELAY)
                .await;
        };
        // Job jalan di task sendiri: panic hanya menggagalkan job ini, bukan worker-nya.
        let (job_queue, job) = (queue.clone(), delivery.job.clone());
        let runner_for_settle = runner.clone();
        let job_run = async move { runner.run(&job_queue, &job).await };
        let mut handle = tokio::spawn(job_run.in_current_span());
        let result = match tokio::time::timeout(job_timeout, &mut handle).await {
            Ok(Ok(result)) => result,
            Ok(Err(_join_error)) => Err(EngineError::Retryable("job panicked".into(), None)),
            Err(_elapsed) => {
                // Hentikan job, supaya tidak jalan terus setelah lock-nya dilepas.
                handle.abort();
                Err(EngineError::Retryable("job timed out".into(), None))
            }
        };
        let settled = settle(queue, runner_for_settle.as_ref(), &delivery, result).await;
        // Lock selalu dilepas, juga saat menyelesaikan job di antrian gagal.
        let unlocked = queue.unlock(&lock_key, &token).await;
        settled.and(unlocked)
    }
    .instrument(span)
    .await
}

/// Ack, jadwalkan ulang dengan backoff, atau pindahkan ke stream dead sesuai hasil job.
async fn settle(
    queue: &Queue,
    runner: &dyn JobRunner,
    delivery: &Delivery,
    result: Result<(), EngineError>,
) -> Result<(), QueueError> {
    match result {
        Ok(()) => {
            tracing::info!(outcome = "done", "job finished");
            queue.ack(delivery).await
        }
        Err(EngineError::Retryable(error, retry_after)) if delivery.attempt < MAX_ATTEMPTS => {
            // Retry-After dari provider dipakai jika lebih lama dari backoff.
            let delay = retry_after
                .unwrap_or_default()
                .max(backoff(delivery.attempt));
            tracing::warn!(outcome = "retry", %error, delay_secs = delay.as_secs(), "job failed; will retry");
            queue.retry(delivery, delivery.attempt + 1, delay).await
        }
        Err(EngineError::Retryable(error, _)) => {
            let error = format!("{error} (gave up after {} attempts)", delivery.attempt);
            tracing::error!(outcome = "dead", %error, "job failed permanently");
            queue.dead_letter(delivery, &error).await?;
            runner.gave_up(&delivery.job, &error).await;
            Ok(())
        }
        Err(EngineError::Fatal(error)) => {
            tracing::error!(outcome = "dead", %error, "job failed permanently");
            queue.dead_letter(delivery, &error).await
        }
    }
}

/// Perbarui heartbeat `name` tiap 10 dtk sampai `shutdown` dibatalkan.
async fn send_heartbeats(name: String, queue: Queue, shutdown: CancellationToken) {
    let mut interval = tokio::time::interval(HEARTBEAT_EVERY);
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            _ = interval.tick() => {}
        }
        if let Err(err) = queue.heartbeat(&name).await {
            tracing::warn!(worker = %name, error = %err, "could not send heartbeat");
        }
    }
}
