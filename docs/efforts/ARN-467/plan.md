# Implementation plan

1. Reproduce undeclared field mutation and ignored pre-state comparisons using real actors under deterministic simulation.
2. Add opt-in IOA contracts and preserve them in compiled tables.
3. Reproduce generic create, PATCH, PUT and DELETE bypasses through HTTP and direct state APIs, then reject them before mutation.
4. Align reaction simulator field projection with production and exercise actual multi-entity factory contracts.
5. Run parser, transition, actor and HTTP regressions. review and merge the kernel dependency, pin it in TemperPaw, then prove the deployed factory.


## Turso cleanup implementation

1. Qualify official stable packages against the existing schema, transaction API and local/remote connection requirements.
2. Adapt the store and required callers; remove vendored libSQL and its dedicated patch machinery. Preserve existing behavior and data.
3. Run existing store invariants, old-file/restart proof and relevant server e2e. Review the complete diff, record proof/review, merge and deploy through Temper only after qualification.

Foundry and Copy delivery are not waiting on this migration. Local worktree execution is authorized by Rita for this migration.

## Chat-based setup implementation

1. Reuse correlated native elicitation for an argument-free setup tool; isolate consent from execute input.
2. Use the existing authorized administration API only after native human consent; create and verify a fixed nonoperator requester.
3. Bind private persisted identity to server/tenant and restore it on reconnect while retaining the operator solely for human administration and approvals.
4. Verify negative consent, real server provisioning, ordinary two-key approvals, persistence, repeated setup, and failure boundaries. Run the required review panel and publish the connector change.
5. Install the reviewed connector, obtain genuine native setup for each authorized service, verify approvals and caller propagation, then complete the original Foundry SDLC deployment gates.
