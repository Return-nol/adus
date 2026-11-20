//! Antrian job di atas Redis Streams.
//!
//! Key yang dipakai (prefix produksi `adus`):
//! - `{prefix}:jobs`        stream job, dibaca consumer group `adus-workers`
//! - `{prefix}:jobs:dead`   stream job yang gagal permanen
//! - `{prefix}:delayed`     sorted set job retry, score = waktu jatuh tempo (unix ms)
//! - `{prefix}:dedup:{key}` penanda delivery webhook yang sudah pernah dilihat
//! - `{prefix}:lock:{key}`  lock dengan token
//!
//! Tiap entri stream punya field `job` (JSON) dan `attempt` (angka desimal, default 1).

use std::time::Duration;

use adus_core::job::Job;
use redis::aio::ConnectionManager;
use redis::streams::{StreamAutoClaimReply, StreamId, StreamReadReply};
use redis::{AsyncConnectionConfig, Client, Script};
use serde::Serialize;
use uuid::Uuid;

const GROUP: &str = "adus-workers";
/// Entri yang belum di-ack selama ini dianggap milik worker yang mati, lalu diambil alih.
const RECLAIM_IDLE_MS: u64 = 15 * 60 * 1000;
const DEDUP_TTL_SECS: u64 = 86_400;
/// Batas job yang dipindah satu kali `promote_due`.
const PROMOTE_BATCH: u32 = 100;

/// Pindahkan job delayed yang jatuh tempo ke stream. ZREM dulu, XADD hanya jika ZREM
/// berhasil, supaya dua scheduler yang jalan bersamaan tidak menggandakan job.
/// Field `job` di member sudah berupa string JSON, jadi diteruskan apa adanya
/// (decode+encode ulang oleh cjson bisa mengubah angka besar seperti `pr_number`).
const PROMOTE_SCRIPT: &str = r"
local members = redis.call('ZRANGEBYSCORE', KEYS[1], '-inf', ARGV[1], 'LIMIT', 0, ARGV[2])
local moved = 0
for _, member in ipairs(members) do
  if redis.call('ZREM', KEYS[1], member) == 1 then
    local entry = cjson.decode(member)
    redis.call('XADD', KEYS[2], '*', 'job', entry.job, 'attempt', tostring(entry.attempt))
    moved = moved + 1
  end
end
return moved
";

/// Hapus lock hanya jika tokennya masih milik pemanggil.
const UNLOCK_SCRIPT: &str = r"
if redis.call('GET', KEYS[1]) == ARGV[1] then
  return redis.call('DEL', KEYS[1])
end
return 0
";

#[derive(Debug, thiserror::Error)]
pub enum QueueError {
    #[error("redis error: {0}")]
    Redis(#[from] redis::RedisError),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Clone)]
pub struct Queue {
    client: Client,
    /// Koneksi bersama yang tersambung ulang otomatis, untuk semua perintah non-blocking.
    conn: ConnectionManager,
    prefix: String,
}

/// Job yang sedang dikerjakan worker. Wajib diakhiri `ack` atau `dead_letter`.
#[derive(Debug, Clone, PartialEq)]
pub struct Delivery {
    pub id: String,
    pub job: Job,
    pub attempt: u32,
}

/// Member sorted set `{prefix}:delayed`. `id` acak membuat dua job identik tetap dua member.
#[derive(Serialize)]
struct DelayedEntry {
    id: Uuid,
    /// JSON job, disimpan sebagai string (lihat `PROMOTE_SCRIPT`).
    job: String,
    attempt: u32,
}

impl Queue {
    pub async fn connect(url: &str, prefix: &str) -> Result<Queue, QueueError> {
        let client = Client::open(url)?;
        let conn = ConnectionManager::new(client.clone()).await?;
        let queue = Queue {
            client,
            conn,
            prefix: prefix.to_string(),
        };

        let created: Result<(), redis::RedisError> = redis::cmd("XGROUP")
            .arg("CREATE")
            .arg(queue.jobs_key())
            .arg(GROUP)
            .arg("0")
            .arg("MKSTREAM")
            .query_async(&mut queue.conn.clone())
            .await;
        match created {
            Ok(()) => {}
            Err(err) if err.code() == Some("BUSYGROUP") => {} // group sudah ada
            Err(err) => return Err(err.into()),
        }
        Ok(queue)
    }

    /// `true` hanya untuk pemanggilan pertama dengan `dedup_key` ini dalam 24 jam.
    pub async fn first_seen(&self, dedup_key: &str) -> Result<bool, QueueError> {
        let reply: Option<String> = redis::cmd("SET")
            .arg(format!("{}:dedup:{dedup_key}", self.prefix))
            .arg(1)
            .arg("NX")
            .arg("EX")
            .arg(DEDUP_TTL_SECS)
            .query_async(&mut self.conn.clone())
            .await?;
        Ok(reply.is_some())
    }

    pub async fn enqueue(&self, job: &Job) -> Result<(), QueueError> {
        let job_json = serde_json::to_string(job)?;
        let _id: String = redis::cmd("XADD")
            .arg(self.jobs_key())
            .arg("*")
            .arg("job")
            .arg(job_json)
            .arg("attempt")
            .arg(1)
            .query_async(&mut self.conn.clone())
            .await?;
        Ok(())
    }

    /// Simpan job untuk dijalankan lagi setelah `delay`. Baru masuk stream lewat `promote_due`.
    pub async fn enqueue_delayed(
        &self,
        job: &Job,
        attempt: u32,
        delay: Duration,
    ) -> Result<(), QueueError> {
        let entry = DelayedEntry {
            id: Uuid::new_v4(),
            job: serde_json::to_string(job)?,
            attempt,
        };
        let due_ms = now_ms() + delay.as_millis() as i64;
        let _added: i64 = redis::cmd("ZADD")
            .arg(self.delayed_key())
            .arg(due_ms)
            .arg(serde_json::to_string(&entry)?)
            .query_async(&mut self.conn.clone())
            .await?;
        Ok(())
    }

    /// Pindahkan maksimal 100 job delayed yang sudah jatuh tempo ke stream; kembalikan jumlahnya.
    pub async fn promote_due(&self) -> Result<usize, QueueError> {
        let moved: usize = Script::new(PROMOTE_SCRIPT)
            .key(self.delayed_key())
            .key(self.jobs_key())
            .arg(now_ms())
            .arg(PROMOTE_BATCH)
            .invoke_async(&mut self.conn.clone())
            .await?;
        Ok(moved)
    }

    /// Ambil job berikutnya untuk `consumer`, menunggu paling lama `block`.
    /// Entri milik worker lain yang idle lebih dari 15 menit diambil alih lebih dulu.
    /// Entri dengan JSON rusak dipindah ke stream dead lalu dilewati.
    pub async fn next(
        &self,
        consumer: &str,
        block: Duration,
    ) -> Result<Option<Delivery>, QueueError> {
        loop {
            let entry = match self.reclaim_stale(consumer).await? {
                Some(entry) => entry,
                None => match self.read_new(consumer, block).await? {
                    Some(entry) => entry,
                    None => return Ok(None),
                },
            };
            if let Some(delivery) = self.to_delivery(entry).await? {
                return Ok(Some(delivery));
            }
        }
    }

    /// Tandai job selesai dan hapus dari stream.
    pub async fn ack(&self, delivery: &Delivery) -> Result<(), QueueError> {
        let _: () = redis::pipe()
            .atomic()
            .cmd("XACK")
            .arg(self.jobs_key())
            .arg(GROUP)
            .arg(&delivery.id)
            .ignore()
            .cmd("XDEL")
            .arg(self.jobs_key())
            .arg(&delivery.id)
            .ignore()
            .query_async(&mut self.conn.clone())
            .await?;
        Ok(())
    }

    /// Pindahkan job ke `{prefix}:jobs:dead` beserta alasannya, lalu ack.
    pub async fn dead_letter(&self, delivery: &Delivery, error: &str) -> Result<(), QueueError> {
        let job_json = serde_json::to_string(&delivery.job)?;
        self.move_to_dead(&delivery.id, &job_json, delivery.attempt, error)
            .await
    }

    /// Ambil lock `key` selama `ttl`. `Some(token)` jika berhasil, `None` jika dipegang pihak lain.
    pub async fn try_lock(&self, key: &str, ttl: Duration) -> Result<Option<String>, QueueError> {
        let token = Uuid::new_v4().to_string();
        let reply: Option<String> = redis::cmd("SET")
            .arg(self.lock_key(key))
            .arg(&token)
            .arg("NX")
            .arg("PX")
            .arg(ttl.as_millis() as u64)
            .query_async(&mut self.conn.clone())
            .await?;
        Ok(reply.map(|_| token))
    }

    /// Lepas lock, tapi hanya jika `token` masih cocok (lock kedaluwarsa bisa sudah dipegang orang lain).
    pub async fn unlock(&self, key: &str, token: &str) -> Result<(), QueueError> {
        let _deleted: i64 = Script::new(UNLOCK_SCRIPT)
            .key(self.lock_key(key))
            .arg(token)
            .invoke_async(&mut self.conn.clone())
            .await?;
        Ok(())
    }

    /// XAUTOCLAIM satu entri yang idle lebih dari 15 menit.
    async fn reclaim_stale(&self, consumer: &str) -> Result<Option<StreamId>, QueueError> {
        let reply: StreamAutoClaimReply = redis::cmd("XAUTOCLAIM")
            .arg(self.jobs_key())
            .arg(GROUP)
            .arg(consumer)
            .arg(RECLAIM_IDLE_MS)
            .arg("0-0")
            .arg("COUNT")
            .arg(1)
            .query_async(&mut self.conn.clone())
            .await?;
        Ok(reply.claimed.into_iter().next())
    }

    /// XREADGROUP satu entri baru, menunggu paling lama `block`.
    async fn read_new(
        &self,
        consumer: &str,
        block: Duration,
    ) -> Result<Option<StreamId>, QueueError> {
        // Perintah BLOCK menahan seluruh koneksi dan melewati batas waktu respons bawaan
        // (500 ms), jadi tiap pembacaan memakai koneksi sendiri dengan batas waktu `block` + 5 dtk.
        let config =
            AsyncConnectionConfig::new().set_response_timeout(Some(block + Duration::from_secs(5)));
        let mut conn = self
            .client
            .get_multiplexed_async_connection_with_config(&config)
            .await?;
        // BLOCK 0 berarti menunggu selamanya, jadi minimal 1 ms.
        let block_ms = block.as_millis().max(1) as u64;
        let reply: Option<StreamReadReply> = redis::cmd("XREADGROUP")
            .arg("GROUP")
            .arg(GROUP)
            .arg(consumer)
            .arg("COUNT")
            .arg(1)
            .arg("BLOCK")
            .arg(block_ms)
            .arg("STREAMS")
            .arg(self.jobs_key())
            .arg(">")
            .query_async(&mut conn)
            .await?;
        let entry = reply
            .and_then(|reply| reply.keys.into_iter().next())
            .and_then(|key| key.ids.into_iter().next());
        Ok(entry)
    }

    /// Ubah entri stream menjadi `Delivery`. Entri yang JSON-nya tidak terbaca dipindah ke
    /// stream dead (payload mentah + error) dan di-ack, supaya tidak menyumbat antrian.
    async fn to_delivery(&self, entry: StreamId) -> Result<Option<Delivery>, QueueError> {
        let attempt = entry
            .get::<String>("attempt")
            .and_then(|attempt| attempt.parse().ok())
            .unwrap_or(1);
        let job_json = entry.get::<String>("job").unwrap_or_default();
        match serde_json::from_str::<Job>(&job_json) {
            Ok(job) => Ok(Some(Delivery {
                id: entry.id,
                job,
                attempt,
            })),
            Err(err) => {
                tracing::error!(entry_id = %entry.id, error = %err, "unreadable job moved to dead stream");
                self.move_to_dead(&entry.id, &job_json, attempt, &err.to_string())
                    .await?;
                Ok(None)
            }
        }
    }

    /// XADD ke stream dead lalu XACK + XDEL entri asli, dalam satu transaksi.
    async fn move_to_dead(
        &self,
        id: &str,
        job_json: &str,
        attempt: u32,
        error: &str,
    ) -> Result<(), QueueError> {
        let _: () = redis::pipe()
            .atomic()
            .cmd("XADD")
            .arg(format!("{}:dead", self.jobs_key()))
            .arg("*")
            .arg("job")
            .arg(job_json)
            .arg("attempt")
            .arg(attempt)
            .arg("error")
            .arg(error)
            .arg("failed_at")
            .arg(chrono::Utc::now().to_rfc3339())
            .ignore()
            .cmd("XACK")
            .arg(self.jobs_key())
            .arg(GROUP)
            .arg(id)
            .ignore()
            .cmd("XDEL")
            .arg(self.jobs_key())
            .arg(id)
            .ignore()
            .query_async(&mut self.conn.clone())
            .await?;
        Ok(())
    }

    fn jobs_key(&self) -> String {
        format!("{}:jobs", self.prefix)
    }

    fn delayed_key(&self) -> String {
        format!("{}:delayed", self.prefix)
    }

    fn lock_key(&self, key: &str) -> String {
        format!("{}:lock:{key}", self.prefix)
    }
}

/// Jeda sebelum percobaan berikutnya: 10 dtk × 2^(attempt-1); attempt dijepit ke 1..=5.
pub fn backoff(attempt: u32) -> Duration {
    let attempt = attempt.clamp(1, 5);
    Duration::from_secs(10 * 2u64.pow(attempt - 1))
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}
