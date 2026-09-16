# Implementation plan

1. `internal_http_capability_issuer` takes the module name and puts it in the
   delegated security context's `context_attrs` as `module`; a caller with no
   identity becomes `SecurityContext::anonymous()` instead of yielding no
   issuer. Callers: the trigger path and the HttpEndpoint path pass their
   module name; the REPL passes none.
2. A unit test resolves the minted capability and checks the principal is
   unchanged and `context.module` is present, for a resolved caller and for
   an anonymous one.
3. Prove it live the way it failed: boot Genesis on this kernel with its
   object-cache permit scoped to the git modules, push, and watch the ingest
   write succeed where it returned 403 before.
4. Confirmation review round; merge; repin genesis #50.
