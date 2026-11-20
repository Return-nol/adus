CREATE TYPE severity AS ENUM ('info', 'warning', 'critical');
CREATE TYPE knowledge_scope AS ENUM ('global', 'repo');
CREATE TYPE memory_source AS ENUM ('manual', 'pr_comment');

-- Rules = apa yang harus di-flag. Scope global (repository_id NULL) atau per repo.
CREATE TABLE rules (
    id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    scope         knowledge_scope NOT NULL,
    repository_id uuid REFERENCES repositories (id) ON DELETE CASCADE,
    title         text NOT NULL,
    body          text NOT NULL,
    -- glob terhadap path tanpa `/` di depan; NULL = semua file
    path_glob     text,
    severity      severity NOT NULL DEFAULT 'warning',
    enabled       boolean NOT NULL DEFAULT true,
    created_at    timestamptz NOT NULL DEFAULT now(),
    updated_at    timestamptz NOT NULL DEFAULT now(),
    CHECK ((scope = 'global' AND repository_id IS NULL)
        OR (scope = 'repo' AND repository_id IS NOT NULL))
);

CREATE INDEX rules_repository_idx ON rules (repository_id);

-- Memory = konvensi tim yang selalu disertakan di prompt review.
CREATE TABLE memories (
    id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    scope         knowledge_scope NOT NULL,
    repository_id uuid REFERENCES repositories (id) ON DELETE CASCADE,
    content       text NOT NULL,
    source        memory_source NOT NULL DEFAULT 'manual',
    source_url    text,
    created_by    uuid REFERENCES users (id) ON DELETE SET NULL,
    enabled       boolean NOT NULL DEFAULT true,
    created_at    timestamptz NOT NULL DEFAULT now(),
    CHECK ((scope = 'global' AND repository_id IS NULL)
        OR (scope = 'repo' AND repository_id IS NOT NULL))
);

CREATE INDEX memories_repository_idx ON memories (repository_id);
