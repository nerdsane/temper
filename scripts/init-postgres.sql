-- Local development bootstrap for docker-compose Postgres.
--
-- Temper runs its schema migrations from Rust on startup. This file exists so
-- the compose volume mount is a real file; without it Docker creates a
-- directory at this path and the Postgres entrypoint fails during init.
SELECT 1;
