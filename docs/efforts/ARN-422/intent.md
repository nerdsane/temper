# Intent — ARN-422

Linear: https://linear.app/arni-build/issue/ARN-422 (child of ARN-411 epic, related-to ARN-420).

Make Genesis the single source of truth for how apps are installed, so the SDLC deploy loop closes
correctly. An app version published to Genesis must be what actually runs; installing it must be one
semantic operation (same for the agent, CI, or a human) that verifies the new version and rolls back
to the last-good pinned hash on failure; and a redeploy must never silently revert a newer install
back to a stale env pin.

This is the in-kernel layer beneath ARN-420's bash deploy pipeline. ARN-420 orchestrates
deploy→verify→rollback from the outside (Datadog health, Genesis publish); ARN-422 makes the kernel's
own install operation verified-or-reverted, so every caller — the agent tool `temper_publish_app` /
`install_app`, the `/paw/apps/install-from-genesis` endpoint, and startup bootstrap — inherits it.

## The two concrete bugs this closes
1. "My change got erased / errors after redeploy." A redeploy reinstalled the env-pinned bootstrap
   ref over a newer agent-published version (downgrade).
2. A bad publish installs and only fails later at first lazy WASM load (the ~4-min prod break), with
   no automatic revert to the last-good version.

## Success = the live e2e in plan.md passes
Publish a version, install it, verify live; simulate a bad publish and confirm rollback to last-good;
confirm a redeploy does not revert a newer install to a stale pin.
