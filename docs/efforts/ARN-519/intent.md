# Intent

A spec-triggered WASM integration must act as itself. Today it acts as whoever
triggered the transition: `Repository.IngestPack` is triggered by a git push,
a push's caller is anonymous by design, and so `scm_ingest_pack` writes the
object cache as "anonymous" — which a correctly scoped object-cache permit
refuses, and the push fails at `field-overflow PUT … returned HTTP 403`.

ARN-499 (D7) gave HttpEndpoint guests their own module identity for exactly
this reason. This effort applies the same rule to the trigger path, so the
kernel treats an integration the same way whether an HTTP request or a state
transition invoked it. Tracked as [ARN-519](https://linear.app/arni-build/issue/ARN-519),
under [ARN-467](https://linear.app/arni-build/issue/ARN-467); it unblocks
`arni-labs/genesis` PR #50.
