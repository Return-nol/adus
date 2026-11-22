-- Cost controls for agentic reviews: smaller PR-size defaults, a per-review input-token
-- ceiling, and cache token columns in the usage log.

-- The settings row already exists (inserted by 0003), so a new column default alone changes
-- nothing for it. The UPDATEs move the row to the new default only while it still holds the
-- old one; a value an admin already changed is kept.
ALTER TABLE settings ALTER COLUMN max_files_per_review SET DEFAULT 30;
UPDATE settings SET max_files_per_review = 30 WHERE max_files_per_review = 50;

ALTER TABLE settings ALTER COLUMN max_patch_lines_per_file SET DEFAULT 1500;
UPDATE settings SET max_patch_lines_per_file = 1500 WHERE max_patch_lines_per_file = 2000;

ALTER TABLE settings ALTER COLUMN small_pr_max_lines SET DEFAULT 100;
UPDATE settings SET small_pr_max_lines = 100 WHERE small_pr_max_lines = 50;

-- Input tokens one review may send to the model (cached tokens included). 0 = no limit.
ALTER TABLE settings
    ADD COLUMN max_review_input_tokens INTEGER NOT NULL DEFAULT 2000000
        CHECK (max_review_input_tokens BETWEEN 0 AND 100000000);

-- input_tokens keeps meaning "total input, cached included"; these show how much of it was
-- read from or written to the provider's prompt cache.
ALTER TABLE llm_usage
    ADD COLUMN cache_read_tokens INTEGER NOT NULL DEFAULT 0,
    ADD COLUMN cache_write_tokens INTEGER NOT NULL DEFAULT 0;
