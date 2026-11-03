-- The opaque version id of each secret's latest write, served as its HTTP
-- etag for conditional writes. Random per write and never derived from the
-- value, so reading it tells nothing about the secret. The volatile default
-- gives every existing row its own id, and gives one to rows an older release
-- inserts after a rollback, since those inserts do not name the column.
ALTER TABLE tenant_secrets ADD COLUMN IF NOT EXISTS version_id TEXT DEFAULT gen_random_uuid()::text;
