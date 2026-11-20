CREATE TYPE review_status AS ENUM ('queued', 'running', 'done', 'failed', 'skipped');
CREATE TYPE review_trigger AS ENUM ('opened', 'updated', 'command');

-- Satu baris per eksekusi review (termasuk yang dilewati/gagal), supaya terlihat di dashboard.
CREATE TABLE reviews (
    id              uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    repository_id   uuid NOT NULL REFERENCES repositories (id) ON DELETE CASCADE,
    pr_number       bigint NOT NULL,
    -- Diisi setelah PR diambil dari provider; NULL jika gagal sebelum itu.
    base_sha        text,
    head_sha        text,
    trigger         review_trigger NOT NULL,
    status          review_status NOT NULL DEFAULT 'queued',
    error           text,
    skip_reason     text,
    comments_posted integer NOT NULL DEFAULT 0,
    started_at      timestamptz,
    finished_at     timestamptz,
    created_at      timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX reviews_repository_pr_idx ON reviews (repository_id, pr_number);
CREATE INDEX reviews_created_idx ON reviews (created_at);

-- FK yang ditunda dari 0003: usage tetap ada walau review-nya terhapus.
ALTER TABLE llm_usage
    ADD CONSTRAINT llm_usage_review_fk
    FOREIGN KEY (review_id) REFERENCES reviews (id) ON DELETE SET NULL;

CREATE INDEX llm_usage_review_idx ON llm_usage (review_id);
