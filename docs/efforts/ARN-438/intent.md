# ARN-438 - temper side: extend verify-temper, retire the misleading deploy workflow

This is the temper slice of ARN-438 (temper stage-2 completion + stage-3 shadow
extension). The full effort and its rulings live in the Linear issue and the
canonical design chain in `arni-labs/stack` (`docs/efforts/ARN-438/`). Two items
ship here:

1. **Extend the `verify-temper` feature map.** The skill already maps five
   surfaces (serve+odata, spec-cascade, dst-proof, mcp-bridge, observe-ui). Stage-2
   completion means the verification skill covers the kernel's real surfaces so any
   change can be driven and proven locally before it is called done. Extend to the
   full set (~13 files) following the `verify-temperpaw` file shape, and correct
   the two stale numeric claims (the cascade is five levels, not L0-L3; the DST
   coverage is the 13 `dst_*` suites).

2. **Delete `.github/workflows/deploy-observe.yml`.** Rita ruled it out: it is a
   "Deploy Railway" workflow that, when its secrets are absent, prints "Skipping
   Railway deploy" and `exit 0` - a green check that deployed nothing. temper does
   not deploy to production directly; it reaches prod by being pinned into a
   temperpaw release (the ARN-438 pin-bump automation). A workflow that looks like
   temper's deploy leg but is inert is worse than none.

## Why now

The temperpaw pin-bump (item 4) makes explicit that temper's deploy leg is the
temperpaw pin, which removes any reason to keep a direct-deploy workflow that never
actually deploys.

## Not in scope here

Items 1 (multi-repo sweep) and 4 (pin-bump) ship in stack/temperpaw; item 5 (aya
release gating) is a Railway config change. This PR is temper's `.agents/` skill +
one workflow deletion.
