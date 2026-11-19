CREATE TYPE connection_kind AS ENUM ('azure_devops', 'github');
CREATE TYPE connection_status AS ENUM ('ok', 'invalid', 'unverified');

-- Satu PAT. Repo hidup terpisah dari PAT supaya mengganti token tidak menyentuh repo.
CREATE TABLE connections (
    id               uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    kind             connection_kind NOT NULL,
    name             text NOT NULL,
    org_or_owner     text NOT NULL,
    encrypted_token  text NOT NULL,
    token_last4      text NOT NULL,
    status           connection_status NOT NULL DEFAULT 'unverified',
    -- login pemilik PAT, untuk ditampilkan di UI
    pat_owner        text,
    last_verified_at timestamptz,
    created_at       timestamptz NOT NULL DEFAULT now()
);

-- RESTRICT: connection tidak bisa terhapus selagi masih ada baris repo (termasuk yang soft-deleted).
CREATE TABLE repositories (
    id                   uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    connection_id        uuid NOT NULL REFERENCES connections (id) ON DELETE RESTRICT,
    external_id          text NOT NULL,
    -- GitHub: owner login. Azure: organization.
    owner                text NOT NULL,
    -- Azure: GUID project. GitHub: NULL.
    project              text,
    name                 text NOT NULL,
    default_branch       text,
    enabled              boolean NOT NULL DEFAULT true,
    auto_review          boolean NOT NULL DEFAULT true,
    webhook_external_ids text[] NOT NULL DEFAULT '{}',
    -- 32 byte acak (base64url): secret HMAC GitHub atau `?token=` Azure
    webhook_token        text,
    -- Override model review; belum ada FK karena tabel ai_providers dibuat task berikutnya.
    review_provider_id   uuid,
    review_model         text,
    deleted_at           timestamptz,
    created_at           timestamptz NOT NULL DEFAULT now()
);

CREATE UNIQUE INDEX repositories_active_external_idx
    ON repositories (connection_id, external_id)
    WHERE deleted_at IS NULL;
