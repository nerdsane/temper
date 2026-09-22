# Typesafe judgments as Temper preconditions

An IOA action should be able to require a typed judgment over its current context
before changing domain state. The user requested Typesafe/Jev integration for
Choice, Score, and Noul, and explicitly chose to keep it inside the existing
conjunctive `guard = [...]` list with `type = "system_one"`. The guard should
resemble Jev's native request and explicitly supply its `state`.

The kernel owns parsing, typed assertion evaluation, provider dispatch, durable
evidence, and transition admission. Application-specific questions, workflows,
and lifecycle states remain in application specifications. This effort adds no
application or agent behavior to Temper.

An enabled judgment permits an otherwise valid action. A negative judgment,
provider failure, missing evidence, or stale context leaves the entity unchanged.
Retries of the same logical action reuse its recorded outcome instead of asking
the model again. Tenant credentials and caller permissions remain part of the
normal authorization boundary.

The user authorized implementation, installing build tooling, running Temper,
and full end-to-end testing with an API key placed in an ignored local file.
The later PR and CI-remediation requests authorize submitting the implementation
and correcting its CI failures; they do not authorize merging or inventing
review or proof results.

Design authority: [ADR-0178](../../adrs/0178-system-one-guards.md). This effort
directory records the accepted conversational design and existing implementation
plan for the repository's planning gate. It was added during CI remediation;
it does not claim a separate ticket or a newly approved design.
