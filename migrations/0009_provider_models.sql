-- Each AI provider carries its own primary_model (required) and fast_model (optional;
-- NULL falls back to primary_model).
-- Backfill existing providers from settings.default_model or repositories.review_model before
-- dropping those columns.
ALTER TABLE ai_providers
    ADD COLUMN primary_model TEXT NULL,
    ADD COLUMN fast_model TEXT NULL;

UPDATE ai_providers p
SET primary_model = COALESCE(
    (SELECT s.default_model FROM settings s WHERE s.default_provider_id = p.id AND btrim(COALESCE(s.default_model, '')) <> '' LIMIT 1),
    (SELECT r.review_model FROM repositories r WHERE r.review_provider_id = p.id AND btrim(COALESCE(r.review_model, '')) <> '' ORDER BY r.created_at DESC LIMIT 1),
    'default'
);

ALTER TABLE ai_providers
    ALTER COLUMN primary_model SET NOT NULL;

ALTER TABLE settings
    DROP COLUMN default_model;

ALTER TABLE repositories
    DROP COLUMN review_model;
