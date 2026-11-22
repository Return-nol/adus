-- How a PR is reviewed: one generalist pass (default) or several specialist passes.
-- repositories.review_mode NULL = use the global setting.
CREATE TYPE review_mode AS ENUM ('generalist', 'specialists');

ALTER TABLE settings
    ADD COLUMN review_mode review_mode NOT NULL DEFAULT 'generalist';

-- A PR with at most this many changed lines (and few files, outside sensitive paths) is
-- reviewed from the diff only. 0 = never treat a PR as small.
ALTER TABLE settings
    ADD COLUMN small_pr_max_lines INTEGER NOT NULL DEFAULT 100
        CHECK (small_pr_max_lines BETWEEN 0 AND 10000);

-- Input tokens one review may send to the model (cached tokens included). 0 = no limit.
ALTER TABLE settings
    ADD COLUMN max_review_input_tokens INTEGER NOT NULL DEFAULT 2000000
        CHECK (max_review_input_tokens BETWEEN 0 AND 100000000);

-- Smaller PR-size defaults. The settings row already exists (inserted by 0003), so a new
-- column default alone changes nothing for it. The UPDATEs move the row to the new default
-- only while it still holds the old one; a value an admin already changed is kept.
ALTER TABLE settings ALTER COLUMN max_files_per_review SET DEFAULT 30;
UPDATE settings SET max_files_per_review = 30 WHERE max_files_per_review = 50;

ALTER TABLE settings ALTER COLUMN max_patch_lines_per_file SET DEFAULT 1500;
UPDATE settings SET max_patch_lines_per_file = 1500 WHERE max_patch_lines_per_file = 2000;

ALTER TABLE repositories
    ADD COLUMN review_mode review_mode NULL;

-- Each AI provider carries its own models: primary_model for the main review pass and an
-- optional fast_model for cheaper follow-up work (NULL = use primary_model).
-- primary_model is NULL only for providers that had no model before this migration: they
-- need setup, and the engine and doctor treat them as "no model configured" until an admin
-- picks one. Never invent a model name here.
ALTER TABLE ai_providers
    ADD COLUMN primary_model TEXT NULL,
    ADD COLUMN fast_model TEXT NULL;

-- Backfill from the old global model (when this provider was the default), else from the
-- newest per-repo override of a repository that still exists.
UPDATE ai_providers p
SET primary_model = COALESCE(
    (SELECT btrim(s.default_model)
     FROM settings s
     WHERE s.default_provider_id = p.id AND btrim(COALESCE(s.default_model, '')) <> ''
     LIMIT 1),
    (SELECT btrim(r.review_model)
     FROM repositories r
     WHERE r.review_provider_id = p.id
       AND r.deleted_at IS NULL
       AND btrim(COALESCE(r.review_model, '')) <> ''
     ORDER BY r.created_at DESC
     LIMIT 1)
);

-- The old engine ignored a repo override that named a provider but no model and used the
-- global provider and model instead. Keep that behaviour by clearing such overrides.
UPDATE repositories
SET review_provider_id = NULL
WHERE review_provider_id IS NOT NULL
  AND btrim(COALESCE(review_model, '')) = '';

ALTER TABLE settings
    DROP COLUMN default_model;

ALTER TABLE repositories
    DROP COLUMN review_model;

-- input_tokens keeps meaning "total input, cached included"; these show how much of it was
-- read from or written to the provider's prompt cache.
ALTER TABLE llm_usage
    ADD COLUMN cache_read_tokens INTEGER NOT NULL DEFAULT 0,
    ADD COLUMN cache_write_tokens INTEGER NOT NULL DEFAULT 0;
