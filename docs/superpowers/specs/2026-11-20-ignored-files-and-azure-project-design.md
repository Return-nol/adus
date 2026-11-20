# Ignored files (global) + nama project Azure & search repo — Desain

Tanggal: 2026-11-20 · Ukuran: perubahan kecil · Status: menunggu persetujuan

## Intent

1. Saat menambah repo Azure, user melihat `ovalteam/<repo>` — nama **project** (myvalue,
   santika, …) hilang, dan daftar repo yang panjang tidak bisa dicari.
2. Tidak semua file layak di-review (lock file, aset, binary, hasil build). User ingin
   **ignored files** seperti Kodus supaya token tidak terbuang dan review tidak berisik.

## Goals

- **Project/repo.** Repo Azure tampil sebagai `project/repo`; GitHub tetap `owner/repo`.
  Dipakai konsisten di dialog Add repositories, tabel repo, Reviews, pemilih scope Rules, dan
  label Token Usage.
- **Search** di dialog Add repositories.
- **Ignored files global** (satu daftar untuk semua repo) yang bisa diedit admin, dengan
  default setara Kodus.
- **[call]** **Ignored title keywords** global: PR yang judulnya mengandung kata tertentu
  tidak di-review otomatis.

## Non-goals

Daftar ignore per repo / per direktori, file konfigurasi di repo (`.adudu.yml`), re-inclusion
ala `.gitignore` (`!pattern`).

## Approaches

| | Cara | Trade-off | Keputusan |
|---|---|---|---|
| **A** | Satu daftar global di `settings` (DB), diedit di halaman Settings | Satu tempat, langsung berlaku, mudah diperluas ke per-repo nanti | **Dipilih** |
| B | File konfigurasi di repo | Per repo, bukan global | Ditolak |
| C | Daftar tetap di kode + tambahan user | Default tak bisa dihapus (mis. `**/*.json`) | Ditolak |

## Desain

### 1. Nama project Azure + search

- Azure `list_repos` sudah menerima `project.name`; simpan sebagai field baru
  `RemoteRepo.project_name: Option<String>` dan kolom baru `repositories.project_name text NULL`.
- **Backfill [call]:** repo Azure lama terisi `project_name` saat daftar remote repo dibuka
  atau saat "Resync webhooks" (update baris yang cocok berdasarkan `external_id`).
- API mengembalikan `display_name` (satu aturan di server): Azure `project_name/name`
  (fallback `owner/name` bila `project_name` kosong), GitHub `owner/name`. Semua halaman memakai
  `display_name`, tidak merakit sendiri.
- Dialog Add repositories: input **Search** (filter di browser, case-insensitive, atas
  `display_name`), teks "N of M repositories", daftar diurutkan per `display_name`.

### 2. Ignored files (global)

- **Default:** daftar `ignorePaths` dari `default-kodus-config.yml` Kodus (±1.100 pola: lock file,
  `.env`, `**/*.json`, media, font, arsip, binary, file IDE). Bagian Kodus ini AGPL (bukan EE),
  disalin apa adanya ke `crates/core/src/default_ignore.rs` dengan komentar atribusi.
- **Penyimpanan [call]:** `settings.ignore_paths text[] NULL` — `NULL` berarti "pakai default
  bawaan", sehingga daftar 1.100 pola tidak diduplikasi di migrasi dan "Reset to defaults" cukup
  menyetel `NULL`. Begitu admin menyimpan perubahan, daftar lengkap tersimpan.
- **Pencocokan:** engine yang sama dengan Rules (`globset` default, path dinormalisasi tanpa
  `/` depan; `*` boleh melewati `/`). Dikompilasi sekali per review menjadi satu `GlobSet`.
- **Engine:** file yang cocok dilewati **sebelum** dikirim ke LLM (review penuh maupun
  inkremental), tidak dihitung ke `max_files_per_review`. Ringkasan review menambahkan baris
  "N files ignored" (tanpa daftar nama).
- **API (admin):**
  - `GET /settings` menambah `ignore_paths` (daftar efektif), `ignore_paths_is_default`,
    `ignored_title_keywords`.
  - `PUT /settings` menerima `ignore_paths` (array, atau `null` = reset) dan
    `ignored_title_keywords`. Validasi: tiap pola glob valid (422 `invalid_glob` menyebut
    polanya), trim, buang duplikat/kosong, maks 500 karakter per pola, maks 5.000 pola.
  - `POST /settings/ignore-test {path}` → `{ ignored: bool, pattern: string|null }` untuk kotak
    "Test a file".
- **UI Settings** (mengikuti DESIGN.md): bagian **Ignored files** — textarea satu pola per baris
  dengan jumlah pola, error validasi inline, tombol **Reset to defaults**, dan kotak
  **Test a file** (ketik path → "Ignored by `pattern`" / "Will be reviewed").

### 3. Ignored title keywords **[call]**

- `settings.ignored_title_keywords text[] NOT NULL DEFAULT '{}'`, maks 100 karakter per kata.
- Judul PR mengandung salah satu kata (case-insensitive) → review `skipped` dengan alasan
  `ignored_title`, tanpa komentar. **[call]** `@adudu review` tetap me-review (perintah eksplisit
  mengalahkan filter).
- UI: input daftar kata di bagian yang sama.

## Diserahkan ke builder

Bentuk komponen UI detail, nama migrasi, susunan test (minimal: default ter-load, pola tidak
valid ditolak, file cocok dilewati dan tidak terhitung batas file, ignore-test, title keyword
skip vs `@adudu review`, `display_name` Azure dengan/tanpa `project_name`, backfill).

## Urutan

Dikerjakan setelah re-review gelombang perbaikan akhir v1 selesai, di branch yang sama.
