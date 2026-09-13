# Implementation plan

1. Read what the kernel already has before writing anything: the durable
   `policies` rows and `load_and_activate_tenant_policies`, which recomposes
   the engine from them with per-row policy ids.
2. Write the consumer as a `BoundActionHook` on `Policy.Activate` / `Revoke`:
   validate, write or disable the row, recompose. No subscriber, no sweep, no
   text surgery.
3. Share the platform's single hook slot with the Genesis install hook through a
   small dispatcher keyed on entity type.
4. Unit-test the refusals: no store, and non-terminal actions being no-ops.
5. Prove it live on a fresh database: approved-not-active is not in force;
   Activate installs; survives a restart; Revoke removes; survives a restart;
   an unparseable statement is refused by Activate and the server stays healthy.
