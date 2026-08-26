# Observe UI

## Sub-features
Web UI on the serve port: entity browser, pending Cedar decisions, approval flow (`temper decide` is the CLI equivalent).

## How to get to it (user POV)
Browser at `http://localhost:<port>/observe`. Auth-gated: unauthenticated requests 401 (that is fail-closed, not breakage).

## Driving it
Browser tooling against /observe after authenticating; or drive the decision flow headlessly: trigger a Cedar-denied action over OData, list pending decisions, approve, re-invoke.

## Gotchas
`/observe/health` is behind the same auth - use `/healthz` for liveness. A denial that never surfaces as a pending decision is a product finding, not a driver error.
