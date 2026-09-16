# Integration identity on the trigger path

When the kernel invokes a WASM integration because an entity transitioned, the
integration's internal HTTP capability — the channel its blob reads and
writes travel on — carries the integration's own module identity,
`Agent::"<module>"`, not the identity of the principal that caused the
transition.

This is the rule ARN-499 D7 already established for HttpEndpoint guests. The
two invocation paths now agree: a module is the same principal however it was
started, and a tenant's Cedar policy names that module directly to grant it
reach. Nothing an integration can do is widened by this; a call that was
denied to `Agent::"scm_ingest_pack"` before is still denied. What changes is
that a call the policy grants to that module is no longer refused because the
module was wearing its caller's identity.

The TData host a triggered integration reads through keeps the caller's
context. D7 left it alone on the endpoint path for a stated reason (a guest
that expects the kernel to scope its reads), and this effort leaves it alone
for the same one.
