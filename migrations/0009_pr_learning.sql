-- Memories learned from closed PRs. A learned memory may wait for an admin (`pending`),
-- be in use (`active`) or be turned down (`rejected`, kept so it is not proposed again).
-- Only `active` memories reach review prompts. Existing memories become `active`.
-- (sqlx runs this file in one transaction; `ADD VALUE` is fine there because nothing below
-- uses 'pr_learning'.)
ALTER TYPE memory_source ADD VALUE 'pr_learning';
CREATE TYPE memory_status AS ENUM ('pending', 'active', 'rejected');
ALTER TABLE memories ADD COLUMN status memory_status NOT NULL DEFAULT 'active';

-- Per repository: `off`, `approval` (learned memories start `pending`) or `auto`.
CREATE TYPE learning_mode AS ENUM ('off', 'approval', 'auto');
ALTER TABLE repositories ADD COLUMN learning_mode learning_mode NOT NULL DEFAULT 'approval';

-- One learning run per PR head, so a repeated close event is not learned twice.
CREATE TABLE pr_learning_runs (
    id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    repository_id uuid NOT NULL REFERENCES repositories (id) ON DELETE CASCADE,
    pr_number     bigint NOT NULL,
    head_sha      text NOT NULL,
    outcome       text NOT NULL,          -- learned | nothing_to_learn | skipped | failed
    learned_count integer NOT NULL DEFAULT 0,
    created_at    timestamptz NOT NULL DEFAULT now(),
    UNIQUE (repository_id, pr_number, head_sha)
);
