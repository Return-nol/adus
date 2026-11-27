-- Thread conversations and curated learning.
CREATE TYPE thread_outcome AS ENUM ('open', 'fixed', 'wont_fix', 'by_design');
CREATE TYPE memory_kind AS ENUM ('convention', 'exception', 'fix_pattern');

-- One row per finding thread Adus opened.
CREATE TABLE review_threads (
    id                 uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    repository_id      uuid NOT NULL REFERENCES repositories (id) ON DELETE CASCADE,
    review_id          uuid REFERENCES reviews (id) ON DELETE SET NULL,
    pr_number          bigint NOT NULL,
    provider_thread_id text NOT NULL,   -- Azure thread id / GitHub root review comment id (GraphQL node id is looked up, never stored)
    root_comment_id    text NOT NULL,   -- Azure comment id within the thread / GitHub review comment id
    path               text NOT NULL,
    line               integer,
    severity           severity NOT NULL,
    title              text NOT NULL,
    body               text NOT NULL,   -- finding body as posted (redacted)
    posted_head_sha    text NOT NULL,
    outcome            thread_outcome NOT NULL DEFAULT 'open',
    reason             text,            -- developer's stated reason (won't fix / by design)
    fix_sha            text,            -- head where Adus confirmed the fix
    resolution         text,            -- how it was fixed / what is left, Adus's words
    adus_replies       integer NOT NULL DEFAULT 0,
    validated_head_sha text,            -- last head push validation checked this thread at
    created_at         timestamptz NOT NULL DEFAULT now(),
    updated_at         timestamptz NOT NULL DEFAULT now(),
    UNIQUE (repository_id, pr_number, provider_thread_id)
);
CREATE INDEX review_threads_open_idx ON review_threads (repository_id, pr_number) WHERE outcome = 'open';

ALTER TABLE memories
    ADD COLUMN kind          memory_kind NOT NULL DEFAULT 'convention',
    ADD COLUMN context       text,        -- curated summary of the discussion
    ADD COLUMN reason        text,
    ADD COLUMN resolution    text,        -- fix applied / recommended fix
    ADD COLUMN evidence      jsonb NOT NULL DEFAULT '[]',  -- [{"url": "...", "pr_number": n}]
    ADD COLUMN supersedes    uuid REFERENCES memories (id) ON DELETE SET NULL,
    ADD COLUMN superseded_by uuid REFERENCES memories (id) ON DELETE SET NULL;

-- Learned memories from the 2026-11-23 wave are by-design exceptions.
UPDATE memories SET kind = 'exception' WHERE source = 'pr_learning';

ALTER TABLE settings
    ADD COLUMN max_thread_replies   integer NOT NULL DEFAULT 5,   -- 0 = Adus never replies in threads
    ADD COLUMN max_validate_threads integer NOT NULL DEFAULT 30,  -- 0 = no validation on push
    ADD COLUMN max_learned_per_pr   integer NOT NULL DEFAULT 10 CHECK (max_learned_per_pr >= 1),
    ADD COLUMN curator_effort       text    NOT NULL DEFAULT 'medium'
        CHECK (curator_effort IN ('low', 'medium', 'high'));
