# Adus — Desain v1

Tanggal: 2026-11-19
Status: draft, menunggu persetujuan

## 1. Intent

Adus adalah platform AI code review self-hosted, open-source, terinspirasi dari
[Kodus](https://github.com/kodustech/kodus-ai) (installer:
[kodus-installer](https://github.com/kodustech/kodus-installer)). Bot-nya bernama
**`@adudu`** (padanan `@kody` di Kodus).

Kenapa dibuat:

- Pemilik ingin punya project open-source sendiri.
- Kodus terasa kurang di beberapa hal yang dipakai sehari-hari (lihat Goals a–b).
- Stack yang nyaman bagi pemilik: **Rust** (backend) + **SvelteKit + shadcn-svelte** (dashboard).
- Project ini juga sarana pemilik **belajar Rust**.

Kodus dipakai sebagai **referensi perilaku** (alur webhook, event yang didukung,
cara posting komentar, konsep Rules vs Memory), **bukan sumber kode yang di-port**.

## 2. Goals (v1)

1. **Dashboard** (SvelteKit SPA + shadcn-svelte):
   - **a. Manajemen repo & koneksi.** Kredensial git disimpan sebagai entitas
     *Connection* terpisah dari repo. Ganti PAT = edit Connection; repo yang sudah
     terhubung **tidak** ikut terhapus. (Di Kodus, ganti PAT memaksa hapus semua
     repo terintegrasi — ini masalah utama yang diperbaiki.)
   - **b. Manajemen AI provider.** Satu provider bisa punya **banyak API key**,
     termasuk provider OpenAI-compatible. (Di Kodus, setelah menambah key
     OpenAI-compatible tidak bisa menambah key lagi.)
   - **c. Manajemen Review Rules dan Memory.**
   - **d. User management** dengan role `admin` dan `user`.
   - **e. Monitoring token usage.**
2. **Webhook + review engine** untuk **Azure DevOps Services** (`dev.azure.com`)
   dan **GitHub**, keduanya via **PAT**: PR dibuka/diupdate → Adus me-review →
   `@adudu` memposting komentar di PR.

## 3. Non-goals (v1)

- GitHub App (menyusul sebagai jenis Connection baru; desain data harus memungkinkan
  ini tanpa mengubah tabel repo).
- Azure DevOps **Server** (versi on-prem yang di-install sendiri). Yang didukung v1
  adalah Azure DevOps **Services** — cloud resmi Microsoft di `dev.azure.com` —
  dengan setup via PAT.
- GitLab, Bitbucket, Forgejo.
- Percakapan dengan `@adudu` (menjawab pertanyaan, membalas di thread saran).
  v1 hanya perintah; percakapan adalah improvement berikutnya.
- SSO/SAML, OAuth login, multi-organisasi/multi-tenant (lihat 6.1).
- Biaya (USD) per token — v1 hanya menghitung token.
- Fitur Kodus lain: custom agents, sandbox (e2b), MCP, CLI, analytics cockpit,
  auto-generate rules dari histori review, integrasi Jira/Linear, approval PR otomatis.
- Helm chart / Kubernetes, template Railway (lihat 6.12).
- Horizontal scaling API/worker terpisah (struktur crate memungkinkan ini nanti).

## 4. Anti-goals

- **Port baris-per-baris dari Kodus.** Menerjemahkan kode NestJS ke Rust bukan
  tujuannya dan merusak tujuan belajar.
- **Menurunkan implementasi dari kode Enterprise Kodus** (`libs/ee/**`, file
  `*.ee.*`). Lisensinya melarang modifikasi dan penggabungan ke produk lain. Fitur
  yang di Kodus berstatus Enterprise (mis. service Kody Rules, SSO,
  linked repositories) **boleh** dibangun ulang di Adus dengan pendekatan
  clean-room (lihat 6.13): spesifikasi hanya dari dokumentasi publik dan perilaku
  yang teramati, tidak pernah dari kode EE.
- **Infra berat.** Tidak ada MongoDB, RabbitMQ, atau microservice. Deploy = `adus`
  + Postgres + Redis.
- **Rust yang terlalu "pintar".** Kode harus bisa dibaca pemula Rust: hindari
  generic/trait bertingkat, macro buatan sendiri, dan abstraksi untuk satu
  implementasi. Pakai trait hanya di batas yang memang punya >1 implementasi
  (git provider, LLM provider).

## 5. Approaches yang dipertimbangkan

| | Bentuk | Trade-off | Keputusan |
|---|---|---|---|
| **A** | Satu binary Rust (API + webhook + worker), SvelteKit SPA di-embed | Paling ringan untuk self-host dan belajar; API/worker tidak bisa di-scale terpisah (bisa ditambah `--role` nanti) | **Dipilih** |
| B | Multi-service seperti Kodus (api, webhooks, worker + broker) | Scale per bagian; ops dan debugging jauh lebih berat | Ditolak |
| C | Seperti A, tapi SvelteKit SSR (Node server) | Bisa server routes/form actions; dua runtime di produksi, dashboard tidak butuh SSR | Ditolak |

Modifikasi dari pemilik atas A: **antrian job pakai Redis**, bukan tabel Postgres.

## 6. Keputusan desain

Bagian bertanda **[call]** adalah keputusan perancang yang belum dikonfirmasi
pemilik secara eksplisit; boleh dikoreksi.

### 6.1 Arsitektur

- Cargo workspace, satu binary `adus`:
  - `crates/core` — tipe domain, error, config.
  - `crates/db` — `sqlx` (Postgres), migrasi (`sqlx migrate`), repository query.
  - `crates/git` — trait `GitProvider` + implementasi `azure`, `github`.
  - `crates/llm` — trait `LlmClient` + implementasi + rotasi key + pencatatan usage.
  - `crates/review` — engine review (diff → prompt → saran → komentar).
  - `crates/queue` — wrapper Redis Streams.
  - `crates/server` — `axum`: REST API dashboard, endpoint webhook, auth, serve SPA.
  - `crates/adus` (bin) — main: menjalankan HTTP server + worker loop dalam satu
    proses `tokio`.
- Library inti: `tokio`, `axum`, `sqlx`, `reqwest`, `redis` (redis-rs), `serde`,
  `tracing`, `argon2`, `aes-gcm`, `similar` (diff), `rust-embed` (SPA).
- **[call]** Satu instalasi = satu organisasi. Tidak ada tabel organization/team di
  v1. Semua user, connection, provider milik instalasi tersebut.
- Lisensi: **AGPL-3.0** **[call]** — karena perilaku dan alur diambil dari Kodus
  (AGPL), ini pilihan paling aman. Pemilik perlu memastikan.

### 6.2 Keamanan data

- PAT dan API key LLM dienkripsi at-rest dengan AES-256-GCM; master key dari env
  `ADUS_SECRET_KEY`. API **tidak pernah** mengembalikan nilai rahasia — hanya
  `last4` dan status.
- Password user di-hash dengan argon2.
- **[call]** Auth dashboard: email + password, session cookie (`HttpOnly`,
  `SameSite=Lax`, `Secure` saat HTTPS), session disimpan di Postgres.
- **[call]** User pertama yang mendaftar menjadi `admin`; setelah itu registrasi
  publik tertutup dan user baru hanya dibuat oleh admin.

### 6.3 Model data (garis besar)

- `users` (id, email, password_hash, role `admin|user`, disabled, github_login
  nullable, created_at)
- `sessions`
- `connections` (id, kind `azure_devops|github`, name, org_or_owner, base_url,
  encrypted_token, token_last4, status `ok|invalid|unverified`, last_verified_at,
  pat_owner — login/identitas pemilik PAT, untuk ditampilkan di UI)
- `repositories` (id, connection_id, external_id, project (Azure), name,
  default_branch, enabled, auto_review, webhook_external_id, webhook_secret,
  review_model_override nullable)
- `ai_providers` (id, kind `openai|anthropic|gemini|openai_compatible`, name,
  base_url, enabled)
- `ai_keys` (id, provider_id, label, encrypted_key, key_last4, enabled,
  status `ok|invalid|rate_limited`, cooldown_until, last_used_at)
- `settings` — default review model (provider_id + model name)
- `rules` (id, scope `global|repo`, repository_id nullable, title, body,
  path_glob nullable, severity `info|warning|critical`, enabled)
- `memories` (id, scope `global|repo`, repository_id nullable, content,
  source `manual|pr_comment`, source_url nullable, created_by, enabled)
- `reviews` (id, repository_id, pr_number, base_sha, head_sha, trigger
  `opened|updated|command`, status `queued|running|done|failed|skipped`, error,
  started_at, finished_at, comments_posted)
- `llm_usage` (id, review_id nullable, repository_id nullable, provider_id, key_id,
  model, input_tokens, output_tokens, latency_ms, success, error_kind, created_at)

### 6.4 Connection & repo (Goal a)

- Tambah Connection: input PAT → Adus memverifikasi dengan memanggil API provider
  (ambil identitas user) sebelum menyimpan.
- **Ganti PAT:** edit Connection → verifikasi token baru → simpan. Repo, review,
  rules, memory tetap. Setelah ganti, Adus memeriksa webhook tiap repo masih ada;
  yang hilang dibuat ulang. Ada tombol **"Resync webhooks"** per Connection.
- Tambah repo: daftar repo diambil dari provider via Connection → pilih → Adus
  mendaftarkan webhook:
  - GitHub: webhook per repo, event `pull_request` + `issue_comment` +
    `pull_request_review_comment`, secret per repo (HMAC `X-Hub-Signature-256`).
  - Azure: service hook subscription per repo untuk
    `git.pullrequest.created`, `git.pullrequest.updated`,
    `ms.vss-code.git-pullrequest-comment-event`. Azure tidak menandatangani
    payload; URL webhook membawa token rahasia per repo di query string (pola yang
    sama dengan Kodus).
- Hapus repo: hapus webhook di provider (best-effort) lalu soft-delete di DB.
- Scope PAT minimum didokumentasikan di UI saat menambah Connection.

### 6.5 AI provider & key (Goal b)

- Provider: OpenAI, Anthropic, Gemini, OpenAI-compatible (base_url bebas — mencakup
  OpenRouter, Ollama, vLLM, dsb). Tiap provider bisa punya **N key**, tanpa batas.
- Tombol **"Test key"** memanggil endpoint ringan provider.
- **Rotasi key [call]:** round-robin di antara key yang `enabled` dan tidak dalam
  cooldown. Saat:
  - 429 / quota habis → key cooldown (pakai `Retry-After` jika ada, default 60 dtk),
    coba key berikutnya.
  - 401/403 → key ditandai `invalid` (tidak dipakai sampai diedit), coba key berikutnya.
  - 5xx/timeout → retry dengan key berikutnya, maks sekali per key.
  - Semua key gagal → panggilan gagal; job di-retry oleh antrian.
- Model review: default global (provider + nama model) di Settings, bisa
  di-override per repo.

### 6.6 Rules & Memory (Goal c)

Mengikuti pembedaan Kodus:

- **Rules** = apa yang harus di-flag. Scope global atau per repo, opsional
  `path_glob` (rule hanya berlaku untuk file yang cocok), severity.
- **Memory** = konteks/konvensi tim yang selalu disertakan dalam prompt. Scope
  global atau per repo. Dibuat manual di dashboard atau lewat komentar PR
  `@adudu remember: <isi>` (scope repo PR tersebut, langsung aktif; bot membalas
  konfirmasi). Hanya dari user terdaftar — lihat 6.10 "Perintah".
- v1 tidak melakukan dedup/resolusi konflik memory otomatis.

### 6.7 User management (Goal d)

| Aksi | admin | user |
|---|---|---|
| Lihat semua halaman | ✓ | ✓ |
| Kelola rules & memory | ✓ | ✓ |
| Kelola connection, repo, AI provider/key, settings | ✓ | ✗ |
| Kelola user | ✓ | ✗ |
| Lihat token usage | ✓ | ✓ |

**[call]** Pembagian di atas. Admin membuat user dengan password awal; user bisa
ganti password sendiri. Minimal satu admin aktif harus selalu ada.

### 6.8 Token usage (Goal e)

- Setiap panggilan LLM menulis satu baris `llm_usage` (sukses maupun gagal).
- Halaman: total input/output token per hari (grafik), breakdown per repo, per
  provider, per key, per model; filter rentang tanggal. Daftar review terakhir
  dengan token per review.

### 6.9 Alur webhook & antrian

1. `POST /webhooks/github/{repo_id}` atau `POST /webhooks/azure/{repo_id}?token=…`.
2. Verifikasi (HMAC GitHub / token Azure). Gagal → 401/403.
3. Filter event yang didukung; event lain → 200 dan diabaikan.
4. Dedup delivery (`SET NX` di Redis, TTL 24 jam, kunci dari delivery id GitHub /
   `id` notifikasi Azure).
5. `XADD adus:jobs` dengan job `{type, repo_id, payload ringkas}`, respons 200 cepat.
6. Worker: `XREADGROUP` consumer group `adus-workers`; `XACK` setelah selesai.
   Job yang tidak di-ack lewat batas waktu diambil ulang (`XAUTOCLAIM`).
   Maks 5 percobaan dengan backoff eksponensial; setelah itu dipindah ke stream
   `adus:jobs:dead` dan review ditandai `failed`.
7. **[call]** Satu PR hanya diproses satu job pada satu waktu (lock Redis per
   `repo_id:pr_number`); job yang lebih baru untuk PR yang sama menunggu/di-requeue.

Tipe job: `review_pr`, `pr_comment` (command `@adudu`).

### 6.10 Review engine

- Pemicu:
  - PR dibuka (bukan draft) dan `auto_review` aktif → review penuh.
  - PR diupdate dengan commit baru → **[call]** review inkremental: diff antara
    `head_sha` review terakhir dan head baru (pola Kodus). Jika tidak ada review
    sebelumnya → review penuh.
  - Komentar `@adudu review` → review penuh ulang.
- **Perintah (`pr_comment`)** — v1 hanya perintah, tanpa percakapan:
  - Dikenali dari teks literal `@adudu` (case-insensitive) di awal perintah, bukan
    dari mention akun (di Azure, mention tersimpan sebagai GUID). Perintah v1:
    `@adudu review`, `@adudu remember: <isi>`. Teks lain setelah `@adudu` →
    bot membalas daftar perintah yang tersedia.
  - **Otorisasi:** hanya penulis komentar yang terdaftar sebagai user Adus
    (tidak `disabled`) yang dilayani:
    - Azure: cocokkan `uniqueName`/email penulis komentar dengan `users.email`.
    - GitHub: email penulis sering tidak publik, jadi **[call]** cocokkan login
      GitHub penulis dengan `users.github_login` (diisi user di halaman Profil
      atau oleh admin); bila email publik tersedia, email juga dicocokkan.
    - **[call]** Penulis tidak terdaftar → diabaikan tanpa balasan (hanya dicatat
      di log), supaya bot tidak spam di repo publik.
  - **Komentar milik Adus sendiri:** setiap komentar yang diposting Adus memuat
    penanda tersembunyi `<!-- adudu -->`; komentar dengan penanda itu selalu
    diabaikan. Dengan begitu PAT boleh milik akun khusus `adudu`
    (direkomendasikan) maupun akun developer biasa, tanpa mengabaikan komentar
    developer tersebut.
- Langkah:
  1. Ambil file yang berubah + patch. GitHub menyediakan patch per file. **Azure
     tidak menyediakan unified diff** — ambil isi file di base dan head commit lalu
     hitung diff lokal dengan `similar`.
  2. Lewati file biner, file terhapus, dan file di atas batas ukuran **[call]**
     (mis. > 2.000 baris diff per file; batas total per PR dapat dikonfigurasi).
  3. Susun prompt per file (atau batch file kecil): instruksi sistem + memory
     (global + repo) + rules yang `path_glob`-nya cocok + diff.
  4. LLM diminta mengembalikan JSON: `[{file, line, severity, title, body,
     suggestion?}]`. Parse ketat; respons tidak valid → retry sekali, lalu file
     dilewati dengan catatan.
  5. Validasi: `line` harus berada di baris yang ditambah/diubah pada diff; yang
     tidak valid dibuang.
  6. Posting:
     - GitHub: satu Pull Request Review berisi komentar inline.
     - Azure: satu thread per saran dengan `threadContext` (file + baris kanan).
     - Satu komentar ringkasan di level PR (jumlah saran per severity).
  7. Simpan hasil di `reviews`.
- Prompt sistem dan format output adalah milik Adus (ditulis sendiri, bukan
  disalin dari Kodus).

### 6.11 Dashboard

- SvelteKit `adapter-static` (SPA), shadcn-svelte, Tailwind. Hasil build di-embed
  ke binary via `rust-embed`; semua route non-`/api` dan non-`/webhooks`
  mengembalikan `index.html`.
- Dev: Vite dev server dengan proxy `/api` ke axum.
- Halaman: Login · Repositories (Connections + repo) · AI Providers (provider +
  keys) · Review Rules (tab Rules / Memory) · Reviews (riwayat) · Token Usage ·
  Users (admin) · Settings (admin) · System status (admin) · Profil (ganti
  password).
- API: REST JSON di `/api/v1/...`.

### 6.12 Deploy & setup (mudah dijalankan)

Pelajaran dari [kodus-installer](https://github.com/kodustech/kodus-installer)
(lisensi MIT): `.env.example` + `generate-secrets.sh` (mint rahasia),
`validate-env.sh` (var wajib, tipe, URL webhook harus `https://`), `install.sh`
(cek Docker/Compose, validasi, `up`, tunggu health, cetak URL), `doctor.sh`
(vonis "Reviews: OK / DEGRADED / NOT RUNNING" lalu daftar masalah + dampak +
perbaikan, termasuk cek dari dalam app: worker mengambil job, panggilan nyata ke
model, token Git dan webhook tiap repo; tidak pernah mencetak rahasia), toggle
DB/broker eksternal (`USE_LOCAL_DB`), init SQL ekstensi, image ter-pin per versi
(`IMAGE_TAG`), dokumentasi port, backup volume, update, troubleshooting. Plus Helm
chart dan template Railway. Adus mengambil pola yang sama, disederhanakan untuk
3 container:

- **Image siap pakai.** GitHub Actions membangun dan mem-push
  `ghcr.io/<owner>/adus:<versi>` (+ `latest`) saat tag `v*`, multi-arch
  (amd64, arm64), supaya pengguna tidak perlu meng-compile Rust. Compose memakai
  `${ADUS_IMAGE:-ghcr.io/<owner>/adus}:${ADUS_VERSION:-latest}`; README
  menyarankan pin versi. **[call]** `<owner>` diisi saat repo GitHub dibuat.
- **`docker-compose.yml`:**
  - `adus` (port `8080`), `postgres:16` + `redis:7` (`appendonly yes`) dengan
    healthcheck dan volume `pgdata`, `redisdata`. DB/Redis tidak dipublish ke host.
  - Postgres dan Redis berada di profile `bundled`; `.env.example` mengisi
    `COMPOSE_PROFILES=bundled`. Pengguna DB/Redis eksternal menghapus profile itu
    dan mengisi `DATABASE_URL` / `REDIS_URL` (padanan `USE_LOCAL_DB` Kodus).
  - Profile opsional `https`: container Caddy dengan HTTPS otomatis untuk
    `ADUS_DOMAIN` (port 80/443) — cara termudah mendapat URL webhook `https://`
    publik.
- **`.env.example` pendek** (±15 variabel), tiap variabel diberi komentar.
- **Subcommand binary** (lintas platform, tanpa `openssl`/bash):
  - `adus serve` (default) — server + worker.
  - `adus gen-secret` — mencetak 32 byte acak base64.
  - `adus check-config` — validasi env tanpa konek DB: variabel wajib ada,
    `ADUS_SECRET_KEY` 32 byte, `ADUS_PUBLIC_URL` URL valid; peringatan bila bukan
    `https://` atau berupa `localhost` (GitHub/Azure tidak bisa mengirim webhook).
  - `adus doctor` — vonis pertama `Reviews: OK | DEGRADED | NOT RUNNING`, lalu
    tiap masalah dengan dampak dan perbaikan: konfigurasi, Postgres + migrasi,
    Redis, heartbeat worker, model default terisi + ada key yang bisa dipakai,
    token tiap Connection valid, webhook tiap repo aktif masih ada. Tidak
    mengubah apa pun, tidak mencetak rahasia; exit code ≠ 0 bila `NOT RUNNING`.
- **Halaman admin "System status"** di dashboard menampilkan cek yang sama
  (`GET /api/v1/admin/doctor`).
- **Skrip install** `scripts/install.sh` (Linux/macOS) dan `scripts/install.ps1`
  (Windows): cek Docker + Compose → salin `.env.example` ke `.env` bila belum ada
  → isi `ADUS_SECRET_KEY` dan `POSTGRES_PASSWORD` yang kosong via
  `adus gen-secret` → `adus check-config` → `docker compose up -d` → tunggu
  `/healthz` (maks 180 dtk) → cetak URL dan langkah berikutnya (buka `/setup`).
  Skrip tidak pernah menimpa nilai yang sudah ada.
- Migrasi DB dijalankan otomatis saat start. Endpoint `/healthz`.
- README: quick start, tabel env, port, backup volume, update versi
  (`ADUS_VERSION` → `docker compose pull && docker compose up -d`),
  troubleshooting (mulai dari `adus doctor`), scope PAT.
- **Non-goal v1:** Helm chart, template Railway, skill instalasi Claude Code,
  skrip pengumpul diagnostik.

### 6.13 Kebijakan clean-room untuk fitur Enterprise Kodus

Tujuan: fitur boleh setara (parity), tapi tidak ada kode, struktur, atau teks
yang diturunkan dari kode Enterprise Kodus.

- **Sumber yang boleh dipakai** untuk menulis spesifikasi fitur: dokumentasi
  publik (docs.kodus.io dan folder `docs/` di repo Kodus — di luar `ee/`),
  README, dan perilaku produk yang teramati saat dijalankan.
- **Sumber yang tidak boleh dibuka** oleh siapa pun yang menulis spesifikasi atau
  kode Adus: `libs/ee/**` dan file `*.ee.*`.
- Alur: tulis spesifikasi fitur (perilaku, input/output, aturan) dari sumber yang
  boleh → simpan di `docs/specs/` beserta daftar sumbernya → implementasi hanya
  dari spesifikasi itu.
- Kode Kodus non-EE (AGPL) boleh dibaca sebagai referensi perilaku; Adus sendiri
  AGPL-3.0, jadi kompatibel.
- Clean-room adalah praktik pengurangan risiko, bukan jaminan hukum. Sebelum rilis
  publik yang memuat fitur-fitur ini, sebaiknya dikonsultasikan ke ahli hukum.

## 7. Diserahkan ke builder

- Versi crate, susunan modul di dalam crate, nama endpoint detail.
- Teks prompt sistem dan strategi batching file.
- Layout visual detail tiap halaman (mengikuti komponen shadcn-svelte standar).
- Strategi test (minimal: unit test untuk pemetaan baris diff, verifikasi
  signature webhook, rotasi key; integration test DB bila praktis).
- Urutan pengerjaan, dengan syarat setiap tahap bisa dijalankan end-to-end.

## 8. Referensi perilaku di Kodus (baca, jangan port; semuanya di luar `ee/`)

- Webhook Azure: `apps/webhooks/src/controllers/azureRepos.controller.ts`
- Webhook GitHub: `apps/webhooks/src/controllers/github.controller.ts`
- Service Azure (subscription, diff, threads): `libs/platform/infrastructure/adapters/services/azureRepos/`
- Tahapan pipeline review: `libs/code-review/pipeline/stages/`
- Rules vs Memory: `docs/en/knowledge_base/difference-between-review-rules-and-memories.mdx`
