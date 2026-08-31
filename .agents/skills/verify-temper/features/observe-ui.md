# Observe UI

## Sub-features
Web UI on the serve port: entity browser, pending Cedar decisions, approval flow. Do **not** treat `temper decide` as the working CLI equivalent today: it polls `?status=Pending` (`decide/mod.rs:122`) while the store writes lowercase `pending`. That is the SQL column (`pending_decisions.status TEXT NOT NULL DEFAULT 'pending'` in turso `schema.rs:156` and postgres `schema.rs:234`), not only `serde(rename_all="lowercase")`. Filed as ARN-442; drive the curl flow in cedar-authz.md with `?status=pending`.

## How to get to it (user POV)
Browser at `http://localhost:<port>/observe`. Auth-gated: unauthenticated requests 401 (that is fail-closed, not breakage).

## Driving it
Browser tooling against /observe after authenticating; or drive the decision flow headlessly: trigger a Cedar-denied action over OData, list pending decisions, approve, re-invoke.

## Gotchas
`/observe/health` is behind the same auth - use `/healthz` for liveness. A denial that never surfaces as a pending decision is a product finding, not a driver error. `temper decide` will list nothing even when pending rows exist (ARN-442) — that is the CLI bug, not an empty store.
