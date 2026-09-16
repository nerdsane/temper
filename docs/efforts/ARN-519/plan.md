# Implementation plan

1. `internal_http_capability_issuer` takes the module name and sets it in
   the delegated security context's `context_attrs` as `module`, or clears
   the key when given none. Callers: the trigger path and the HttpEndpoint
   path pass their module name; the REPL passes none.
2. A unit test resolves the minted capability and checks the principal is
   unchanged, `context.module` is set for this hop or cleared, and a caller
   with no context gets no capability.
3. Prove it live the way it failed: boot Genesis on this kernel with its
   object-cache permit scoped to the git modules, push, and watch the ingest
   write succeed where it returned 403 before.
4. Confirmation review round; merge; repin genesis #50.
