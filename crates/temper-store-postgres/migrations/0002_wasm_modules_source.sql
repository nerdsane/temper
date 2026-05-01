-- 0002_wasm_modules_source.sql
--
-- Add a `source` column to wasm_modules so the os-apps install pipeline can
-- distinguish between rows it owns ('bundled' — bytes from the deploy image)
-- and rows produced by hot uploads via POST /api/wasm/modules/{name}
-- ('upload'). The install pipeline must skip overwriting rows whose source
-- is 'upload' so iterative testing via hot-load survives subsequent
-- restarts. Default is 'bundled' so pre-existing rows behave the same as
-- the install pipeline that originally wrote them.

ALTER TABLE wasm_modules
    ADD COLUMN IF NOT EXISTS source TEXT NOT NULL DEFAULT 'bundled';
