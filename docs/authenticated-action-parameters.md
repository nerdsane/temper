# Authenticated action parameters

An IOA action can bind a string parameter to the authenticated subject:

```toml
[automaton]
name = "Document"
states = ["Requested", "Active"]
initial = "Requested"
strict_action_params = true

[[state]]
name = "owner_id"
type = "string"
initial = ""

[[action]]
name = "Create"
from = ["Requested"]
to = "Active"
params = [{ name = "owner_id", type = "string", source = "authenticated_subject" }]
```

The caller submits `{}`. The runtime resolves `owner_id` from the verified
security context's `acting_for` subject, falling back to its principal ID.
Caller-supplied values are rejected, including a value equal to the real subject.
Missing, anonymous, system and service identities cannot supply this source.
An empty delegated subject is rejected rather than falling back to another owner.
Attribution headers and `AgentContext.agent_id` do not establish identity.

This requires `strict_action_params = true` and a string parameter. Omit the
bound parameter from the caller-facing CSDL action signature. The resolved value
participates in normal action validation, effects and durable event persistence.
Later actions must omit the owner parameter if ownership is immutable.

Binding does not grant permission to create or modify an entity. Cedar still
controls action access against pre-action state. A policy can allow authenticated
creation before an owner exists, then authorize later operations against the
stored owner. This feature does not project related entities into authorization.

The native core dispatcher and OData PostgreSQL actor path resolve inputs before
sending an action to the actor. Composite preflight resolves the same inputs
before atomic writes, while its fallback passes original inputs back to core. Native OData additionally validates a resolved
copy before materialization, then dispatch resolves the original request again.
Trusted actor messages and replay events are internal runtime interfaces, not
untrusted request entry points. Service reactions should inherit already-bound
values using ordinary declared parameters, not attempt to impersonate a caller.

## The agent's label

`source = "authenticated_agent_label"` binds the name a trusted issuer gave the
calling agent instance (its token's `agent_label` claim), so a record can keep
who made it after `last_event_label` has moved on:

```toml
params = [{ name = "author", type = "string", source = "authenticated_agent_label" }]
```

It follows the same rules as the subject: the caller submits `{}`, a supplied
value is rejected even when it is true, and a move with no caller (system,
service, anonymous) is refused. A timer that fires such a move needs a caller
too. It needs `strict_action_params = true` and a string parameter, and `as` on
a carry still takes only the subject. A person in person, or an agent whose
issuer gave it no label, binds an empty string and the move lands: having no
label says nothing about what a caller may do. The kernel keeps the label away
from Cedar, but a bound field is an ordinary field of the record, which the
app's own policies can read like any other.

Regression tests: `cargo test -p temper-server --test authenticated_params` and
`cargo test -p temper-server --lib authenticated_params`.
The tests use actual verification results, OData handlers, a local libSQL
journal, and the PostgreSQL actor runtime. The PostgreSQL regression rejects
caller overrides before enqueueing, executes an accepted action, and reads the
committed owner through a fresh actor system. It starts an isolated PostgreSQL
testcontainer (Docker required), or uses the existing test helper's
`TEMPER_ACTOR_TEST_DATABASE_URL` override for a local database named
`temper_test_*`. No database test is silently skipped.
