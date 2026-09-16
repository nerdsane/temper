# Intent

A policy must be able to name the integration that makes an internal call.
Today a spec-triggered WASM integration's calls back into the kernel carry
only its caller's identity: `Repository.IngestPack` is triggered by a git
push, a push's caller is anonymous by design, and so `scm_ingest_pack` writes
the object cache as "anonymous" with nothing a scoped permit can match — the
push fails at `field-overflow PUT … returned HTTP 403`.

The integration keeps acting for its caller; that is what every tenant's
policy was written against. What it gains is `context.module`, its own name
on the call, so a policy can grant a module reach in its own right. Tracked as
[ARN-519](https://linear.app/arni-build/issue/ARN-519), under
[ARN-467](https://linear.app/arni-build/issue/ARN-467); it unblocks
`arni-labs/genesis` PR #50.
