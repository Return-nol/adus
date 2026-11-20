-- Daftar pola file yang dilewati reviewer. NULL = pakai bawaan (adus_core::default_ignore).
-- Kata kunci judul PR yang membuat review otomatis dilewati.
ALTER TABLE settings
    ADD COLUMN ignore_paths text[] NULL,
    ADD COLUMN ignored_title_keywords text[] NOT NULL DEFAULT '{}';
