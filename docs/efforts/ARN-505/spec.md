# Installing an approved policy

The durable `policies` table is the source of truth for a tenant's Cedar set.
The engine is recomposed from it, one named policy per enabled row; the kernel
already does this at boot and after policy API writes.

`Policy.Activate` validates that the row's `cedar_statement` parses, writes it as
that entity's durable row, and recomposes the tenant's engine. `Policy.Revoke`
disables that row and recomposes. Both run inside the action's dispatch, as a
bound-action hook: the caller's response reports whether the install happened.

A statement that does not parse is refused before it becomes a row, so it can
never poison a later recompose or the next boot. With no durable store
configured, `Activate` refuses rather than installing a policy that a restart
would silently drop.

Ownership of a statement is its row. Two rows with identical text are two
policies with two ids. Bootstrap permits are rows of their own. No code path
edits policy text.

Other `Policy` actions, and every other entity type, are untouched by the hook.
