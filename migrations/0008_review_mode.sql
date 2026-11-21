-- How a PR is reviewed: one generalist pass (default) or several specialist passes.
-- repositories.review_mode NULL = use the global setting.
CREATE TYPE review_mode AS ENUM ('generalist', 'specialists');

ALTER TABLE settings
    ADD COLUMN review_mode review_mode NOT NULL DEFAULT 'generalist';

ALTER TABLE repositories
    ADD COLUMN review_mode review_mode NULL;
