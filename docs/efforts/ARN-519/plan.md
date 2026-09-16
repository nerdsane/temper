# Implementation plan

1. In the trigger dispatch path, bind the internal capability issuer to the
   module's own security context, mirroring the HttpEndpoint path (D7), at both
   sites that currently hand it the caller's context.
2. Say why in the code, next to the D7 comment it mirrors.
3. Prove it live the way it failed: boot Genesis on this kernel with its
   object-cache permit scoped to the git modules, push, and watch the ingest
   write succeed where it returned 403 before.
4. One review round; merge; repin genesis #50.
