# Module name on a guest's internal calls

When a WASM integration calls back into the kernel — over the internal HTTP
capability its blob reads and writes travel on — the call carries two things:
the identity of the principal the integration acts for, unchanged, and
`context.module`, the name of the module making the call.

The principal is what it always was. A spec-triggered integration acts for
whoever caused the transition; an HttpEndpoint guest acts as its module
(ARN-499 D7). Nothing a tenant's existing policy relied on changes, because
no principal is rewritten.

What is new is that a policy can grant a module reach in its own right, by
name, for calls made on behalf of a principal that has no standing of its own.
A public `git push` is that case: the pusher is anonymous by design, the
ingest integration must still write the object cache, and Genesis's
BlobObject permit names `scm_ingest_pack` in `context.module`. A caller
without an identity is therefore the anonymous principal with a capability,
not a call without one — the policy decides, instead of the transport.
