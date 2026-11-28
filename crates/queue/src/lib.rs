//! Antrian job di atas Redis Streams.
//!
//! Key yang dipakai (prefix produksi `adus`):
//! - `{prefix}:jobs`        stream job, dibaca consumer group `adus-workers`
//! - `{prefix}:jobs:dead`   stream job yang gagal permanen
//! - `{prefix}:delayed`     sorted set job retry, score = waktu jatuh tempo (unix ms)
//! - `{prefix}:dedup:{key}` penanda delivery webhook yang sudah pernah dilihat
//! - `{prefix}:lock:{key}`  lock dengan token
//! - `{prefix}:cache:{key}` cache key dengan TTL
//! - `{prefix}:worker:{name}:hb` heartbeat worker (unix detik), kedaluwarsa setelah 30 dtk
//!
//! Tiap entri stream punya field `job` (JSON) dan `attempt` (angka desimal, default 1).

use std::sync::LazyLock;
use std::time::Duration;

use adus_core::job::Job;
use redis::aio::ConnectionManager;
use redis::streams::{StreamAutoClaimReply, StreamId, StreamReadReply};
use redis::{AsyncConnectionConfig, Client, Script};
use serde::Serialize;
use uuid::Uuid;

/// Batas percobaan sebuah job; setelah itu job masuk `{prefix}:jobs:dead`.
pub const MAX_ATTEMPTS: u32 = 5;

const GROUP: &str = "adus-workers";
/// An entry not acked for this long is taken to belong to a dead worker and is taken over.
/// Longer than the worker's job backstop (a Specialists review runs up to 30 min), checked at
/// compile time in `adus::worker`.
pub const RECLAIM_IDLE: Duration = Duration::from_secs(35 * 60);
const RECLAIM_IDLE_MS: u64 = RECLAIM_IDLE.as_secs() * 1000;
const DEDUP_TTL_SECS: u64 = 86_400;
/// Batas job yang dipindah satu kali `promote_due`.
const PROMOTE_BATCH: u32 = 100;
/// Heartbeat yang tidak diperbarui selama ini hilang; worker-nya dianggap mati.
const HEARTBEAT_TTL_SECS: u64 = 30;

/// Pindahkan job delayed yang jatuh tempo ke stream. ZREM dulu, XADD hanya jika ZREM
/// berhasil, supaya dua scheduler yang jalan bersamaan tidak menggandakan job.
/// Field `job` di member sudah berupa string JSON, jadi diteruskan apa adanya
/// (decode+encode ulang oleh cjson bisa mengubah angka besar seperti `pr_number`).
static PROMOTE_SCRIPT: LazyLock<Script> = LazyLock::new(|| {
    Script::new(
        r"
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
",
    )
});

/// Penanda dedup dan XADD dalam satu langkah atomik: job masuk stream tepat sekali per
/// `dedup_key`, dan penanda tidak pernah ada tanpa job-nya (atau sebaliknya).
static ENQUEUE_ONCE_SCRIPT: LazyLock<Script> = LazyLock::new(|| {
    Script::new(
        r"
if redis.call('SET', KEYS[1], '1', 'NX', 'EX', ARGV[1]) then
  redis.call('XADD', KEYS[2], '*', 'job', ARGV[2], 'attempt', '1')
  return 1
end
return 0
",
    )
});

/// Hapus lock hanya jika tokennya masih milik pemanggil.
static UNLOCK_SCRIPT: LazyLock<Script> = LazyLock::new(|| {
    Script::new(
        r"
if redis.call('GET', KEYS[1]) == ARGV[1] then
  return redis.call('DEL', KEYS[1])
end
return 0
",
    )
});

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

    /// Seperti `enqueue`, tetapi hanya jika `dedup_key` belum terlihat dalam 24 jam
    /// (atomik, lihat `ENQUEUE_ONCE_SCRIPT`). `false` = duplikat, tidak ada yang ditambahkan.
    pub async fn enqueue_once(&self, job: &Job, dedup_key: &str) -> Result<bool, QueueError> {
        let added: i64 = ENQUEUE_ONCE_SCRIPT
            .key(self.dedup_key(dedup_key))
            .key(self.jobs_key())
            .arg(DEDUP_TTL_SECS)
            .arg(serde_json::to_string(job)?)
            .invoke_async(&mut self.conn.clone())
            .await?;
        Ok(added == 1)
    }

    /// Simpan job untuk dijalankan lagi setelah `delay`. Baru masuk stream lewat `promote_due`.
    pub async fn enqueue_delayed(
        &self,
        job: &Job,
        attempt: u32,
        delay: Duration,
    ) -> Result<(), QueueError> {
        let _added: i64 = redis::cmd("ZADD")
            .arg(self.delayed_key())
            .arg(due_ms(delay))
            .arg(delayed_member(job, attempt)?)
            .query_async(&mut self.conn.clone())
            .await?;
        Ok(())
    }

    /// Pindahkan maksimal 100 job delayed yang sudah jatuh tempo ke stream; kembalikan jumlahnya.
    pub async fn promote_due(&self) -> Result<usize, QueueError> {
        let moved: usize = PROMOTE_SCRIPT
            .key(self.delayed_key())
            .key(self.jobs_key())
            .arg(now_ms())
            .arg(PROMOTE_BATCH)
            .invoke_async(&mut self.conn.clone())
            .await?;
        Ok(moved)
    }

    /// Ambil job berikutnya untuk `consumer`, menunggu paling lama `block`.
    ///
    /// Entri yang belum di-ack lebih dari `RECLAIM_IDLE` (35 menit) dianggap milik worker yang
    /// mati dan diambil alih lebih dulu. Tiap pengambilalihan menambah `attempt` satu; lewat dari
    /// `MAX_ATTEMPTS` job masuk stream dead. Artinya job wajib di-`ack`, di-`retry` atau
    /// di-`dead_letter` dalam 35 menit (`JOB_TIMEOUT` worker, 33 menit, ada di bawah batas itu;
    /// TTL lock sama, 35 menit), kalau tidak worker lain ikut mengerjakannya.
    ///
    /// Jika future ini dibatalkan tepat saat Redis sudah mengirim entri, entri itu tetap di
    /// daftar pending (PEL) dan diambil alih setelah 35 menit. Entri dengan JSON rusak
    /// dipindah ke stream dead lalu dilewati.
    pub async fn next(
        &self,
        consumer: &str,
        block: Duration,
    ) -> Result<Option<Delivery>, QueueError> {
        loop {
            if let Some(entry) = self.reclaim_stale(consumer).await? {
                // XAUTOCLAIM menaikkan hitungan delivery. Tiap delivery sesudah yang pertama
                // berarti satu worker mati di tengah percobaan.
                let deaths = self.times_delivered(&entry.id).await?.saturating_sub(1);
                if let Some(delivery) = self.to_delivery(entry, deaths).await? {
                    return Ok(Some(delivery));
                }
                continue;
            }
            let Some(entry) = self.read_new(consumer, block).await? else {
                return Ok(None);
            };
            if let Some(delivery) = self.to_delivery(entry, 0).await? {
                return Ok(Some(delivery));
            }
        }
    }

    /// Tandai job selesai dan hapus dari stream. Harus dalam 35 menit sejak `next`
    /// (lihat `next`), kalau tidak job sudah bisa diambil alih worker lain.
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

    /// Jadwalkan ulang job sebagai percobaan `attempt` setelah `delay`, lalu ack entri ini.
    /// ZADD + XACK + XDEL dalam satu transaksi, jadi crash tidak menggandakan atau menghilangkan job.
    pub async fn retry(
        &self,
        delivery: &Delivery,
        attempt: u32,
        delay: Duration,
    ) -> Result<(), QueueError> {
        let _: () = redis::pipe()
            .atomic()
            .cmd("ZADD")
            .arg(self.delayed_key())
            .arg(due_ms(delay))
            .arg(delayed_member(&delivery.job, attempt)?)
            .ignore()
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
            .arg(ttl.as_millis().max(1) as u64) // PX 0 ditolak Redis
            .query_async(&mut self.conn.clone())
            .await?;
        Ok(reply.map(|_| token))
    }

    /// Lepas lock, tapi hanya jika `token` masih cocok (lock kedaluwarsa bisa sudah dipegang orang lain).
    pub async fn unlock(&self, key: &str, token: &str) -> Result<(), QueueError> {
        let _deleted: i64 = UNLOCK_SCRIPT
            .key(self.lock_key(key))
            .arg(token)
            .invoke_async(&mut self.conn.clone())
            .await?;
        Ok(())
    }

    /// Ambil nilai cache string untuk `key`, atau `None` jika tidak ada/kedaluwarsa.
    pub async fn cache_get(&self, key: &str) -> Result<Option<String>, QueueError> {
        let value: Option<String> = redis::cmd("GET")
            .arg(self.cache_key(key))
            .query_async(&mut self.conn.clone())
            .await?;
        Ok(value)
    }

    /// Simpan nilai cache string untuk `key` dengan TTL `ttl` (minimal 1 detik).
    pub async fn cache_set(&self, key: &str, value: &str, ttl: Duration) -> Result<(), QueueError> {
        let secs = ttl.as_secs().max(1);
        let _: () = redis::cmd("SET")
            .arg(self.cache_key(key))
            .arg(value)
            .arg("EX")
            .arg(secs)
            .query_async(&mut self.conn.clone())
            .await?;
        Ok(())
    }

    /// Hapus nilai cache untuk `key` jika ada.
    pub async fn cache_del(&self, key: &str) -> Result<(), QueueError> {
        let _: i64 = redis::cmd("DEL")
            .arg(self.cache_key(key))
            .query_async(&mut self.conn.clone())
            .await?;
        Ok(())
    }

    /// Tandai `worker` masih hidup selama 30 dtk ke depan. Worker memanggilnya tiap 10 dtk.
    pub async fn heartbeat(&self, worker: &str) -> Result<(), QueueError> {
        let _: () = redis::cmd("SET")
            .arg(self.heartbeat_key(worker))
            .arg(chrono::Utc::now().timestamp())
            .arg("EX")
            .arg(HEARTBEAT_TTL_SECS)
            .query_async(&mut self.conn.clone())
            .await?;
        Ok(())
    }

    /// Nama worker yang heartbeat-nya belum kedaluwarsa, urut abjad.
    pub async fn live_workers(&self) -> Result<Vec<String>, QueueError> {
        let head = format!("{}:worker:", self.prefix);
        let pattern = self.heartbeat_key("*");
        let mut workers = Vec::new();
        let mut cursor: u64 = 0;
        loop {
            let (next, keys): (u64, Vec<String>) = redis::cmd("SCAN")
                .arg(cursor)
                .arg("MATCH")
                .arg(&pattern)
                .arg("COUNT")
                .arg(100)
                .query_async(&mut self.conn.clone())
                .await?;
            for key in keys {
                if let Some(name) = key.strip_prefix(&head).and_then(|k| k.strip_suffix(":hb")) {
                    workers.push(name.to_string());
                }
            }
            if next == 0 {
                break;
            }
            cursor = next;
        }
        // SCAN boleh mengembalikan key yang sama lebih dari sekali.
        workers.sort();
        workers.dedup();
        Ok(workers)
    }

    /// Jumlah job di `{prefix}:jobs:dead` (XLEN; 0 jika stream belum ada).
    pub async fn dead_count(&self) -> Result<u64, QueueError> {
        let count: u64 = redis::cmd("XLEN")
            .arg(format!("{}:dead", self.jobs_key()))
            .query_async(&mut self.conn.clone())
            .await?;
        Ok(count)
    }

    /// XAUTOCLAIM satu entri yang idle lebih dari `RECLAIM_IDLE`.
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

    /// Berapa kali entri ini sudah dikirim ke worker (XPENDING).
    async fn times_delivered(&self, id: &str) -> Result<u32, QueueError> {
        // Tiap baris: (id, consumer, idle ms, jumlah delivery).
        let rows: Vec<(String, String, u64, u32)> = redis::cmd("XPENDING")
            .arg(self.jobs_key())
            .arg(GROUP)
            .arg(id)
            .arg(id)
            .arg(1)
            .query_async(&mut self.conn.clone())
            .await?;
        Ok(rows.first().map(|row| row.3).unwrap_or(1))
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

    /// Ubah entri stream menjadi `Delivery`; `deaths` = worker yang mati saat memegang entri ini.
    /// Entri yang JSON-nya tidak terbaca, atau yang sudah melewati `MAX_ATTEMPTS` karena worker
    /// terus mati, dipindah ke stream dead dan di-ack, supaya tidak menyumbat antrian.
    async fn to_delivery(
        &self,
        entry: StreamId,
        deaths: u32,
    ) -> Result<Option<Delivery>, QueueError> {
        let stored_attempt: u32 = entry
            .get::<String>("attempt")
            .and_then(|attempt| attempt.parse().ok())
            .unwrap_or(1);
        let attempt = stored_attempt.saturating_add(deaths);
        let job_json = entry.get::<String>("job").unwrap_or_default();
        match serde_json::from_str::<Job>(&job_json) {
            Ok(_) if deaths > 0 && attempt > MAX_ATTEMPTS => {
                let error = "exceeded max deliveries (worker died)";
                tracing::error!(entry_id = %entry.id, attempt, "{error}; moved to dead stream");
                self.move_to_dead(&entry.id, &job_json, attempt, error)
                    .await?;
                Ok(None)
            }
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

    fn dedup_key(&self, key: &str) -> String {
        format!("{}:dedup:{key}", self.prefix)
    }

    fn delayed_key(&self) -> String {
        format!("{}:delayed", self.prefix)
    }

    fn lock_key(&self, key: &str) -> String {
        format!("{}:lock:{key}", self.prefix)
    }

    fn cache_key(&self, key: &str) -> String {
        format!("{}:cache:{key}", self.prefix)
    }

    fn heartbeat_key(&self, worker: &str) -> String {
        format!("{}:worker:{worker}:hb", self.prefix)
    }
}

/// Jeda sebelum percobaan berikutnya: 10 dtk × 2^(attempt-1); attempt dijepit ke 1..=5.
pub fn backoff(attempt: u32) -> Duration {
    let attempt = attempt.clamp(1, MAX_ATTEMPTS);
    Duration::from_secs(10 * 2u64.pow(attempt - 1))
}

/// Member sorted set delayed untuk `job` sebagai percobaan `attempt`.
fn delayed_member(job: &Job, attempt: u32) -> Result<String, QueueError> {
    let entry = DelayedEntry {
        id: Uuid::new_v4(),
        job: serde_json::to_string(job)?,
        attempt,
    };
    Ok(serde_json::to_string(&entry)?)
}

/// Waktu jatuh tempo (unix ms) `delay` dari sekarang; dipakai sebagai score sorted set.
fn due_ms(delay: Duration) -> i64 {
    now_ms() + delay.as_millis() as i64
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}
