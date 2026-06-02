# Repo-Local OS Apps

Genesis is the canonical source for production Temper-native app bundles.

This `os-apps/` directory is a bootstrap/dev/test surface. It may hold fixtures,
local verification bundles, and the tiny first-boot recovery seed needed before a
running Temper instance can install pinned Genesis refs.

It is not a production app catalog and it is not an app-source mirror.

Production startup should install configured pinned Genesis refs and recover
already-installed app state from the backing Temper store.
