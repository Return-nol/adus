CREATE TYPE provider_kind AS ENUM ('openai', 'anthropic', 'gemini', 'openai_compatible');
CREATE TYPE ai_key_status AS ENUM ('ok', 'invalid', 'rate_limited');

CREATE TABLE ai_providers (
    id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    kind       provider_kind NOT NULL,
    name       text NOT NULL,
    -- NULL = pakai base URL bawaan provider; wajib diisi untuk openai_compatible.
    base_url   text,
    enabled    boolean NOT NULL DEFAULT true,
    created_at timestamptz NOT NULL DEFAULT now()
);

-- Satu provider boleh punya banyak key (tanpa batas, tidak ada UNIQUE pada provider_id).
CREATE TABLE ai_keys (
    id             uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    provider_id    uuid NOT NULL REFERENCES ai_providers (id) ON DELETE CASCADE,
    label          text NOT NULL,
    encrypted_key  text NOT NULL,
    key_last4      text NOT NULL,
    enabled        boolean NOT NULL DEFAULT true,
    status         ai_key_status NOT NULL DEFAULT 'ok',
    cooldown_until timestamptz,
    last_used_at   timestamptz,
    created_at     timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX ai_keys_provider_idx ON ai_keys (provider_id);

-- Override model review per repo (kolom dibuat di migrasi 0002 tanpa FK).
ALTER TABLE repositories
    ADD CONSTRAINT repositories_review_provider_fk
    FOREIGN KEY (review_provider_id) REFERENCES ai_providers (id) ON DELETE SET NULL;

-- Riwayat usage harus bertahan walau key/provider dihapus, jadi FK-nya SET NULL.
-- review_id belum punya FK: tabel `reviews` dibuat task berikutnya.
CREATE TABLE llm_usage (
    id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    review_id     uuid,
    repository_id uuid REFERENCES repositories (id) ON DELETE SET NULL,
    provider_id   uuid REFERENCES ai_providers (id) ON DELETE SET NULL,
    key_id        uuid REFERENCES ai_keys (id) ON DELETE SET NULL,
    model         text NOT NULL,
    input_tokens  integer NOT NULL DEFAULT 0,
    output_tokens integer NOT NULL DEFAULT 0,
    latency_ms    integer NOT NULL,
    success       boolean NOT NULL,
    error_kind    text,
    created_at    timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX llm_usage_created_idx ON llm_usage (created_at);

-- Satu baris saja (id selalu 1).
CREATE TABLE settings (
    id                       smallint PRIMARY KEY DEFAULT 1 CHECK (id = 1),
    default_provider_id      uuid REFERENCES ai_providers (id) ON DELETE SET NULL,
    default_model            text,
    max_files_per_review     integer NOT NULL DEFAULT 50,
    max_patch_lines_per_file integer NOT NULL DEFAULT 2000
);

INSERT INTO settings (id) VALUES (1);
