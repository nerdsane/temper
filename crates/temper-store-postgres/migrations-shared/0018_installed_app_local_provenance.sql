ALTER TABLE tenant_installed_apps
    ADD COLUMN IF NOT EXISTS dependency_lock_digest TEXT NOT NULL DEFAULT '';
