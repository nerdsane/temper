ALTER TABLE tenant_installed_apps
    ADD COLUMN IF NOT EXISTS pinned_version_hash TEXT NOT NULL DEFAULT '',
    ADD COLUMN IF NOT EXISTS current_version_hash TEXT NOT NULL DEFAULT '',
    ADD COLUMN IF NOT EXISTS follow_policy TEXT NOT NULL DEFAULT 'pinned';
