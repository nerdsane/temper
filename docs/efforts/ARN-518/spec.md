# Bounded background callback continuation

## Problem

AgentContext.callback_depth counts every integration callback. A detached task inherits this count despite no longer executing on its parent's inline stack. The eight-level reaction budget therefore becomes an accidental eight-hop lifetime limit for ordinary background workflows.

## Contract

The runtime owns two separate counters:

- Inline callback depth: at most 8, retaining the current recursion limit.
- Total callback hops: at most 512 per server-owned context lineage.

Admitting a callback increments both counters after checking both budgets. Refusal has no execution effect and remains observable through the existing integration_callback_rejected event.

At an existing detached-task boundary, the child context starts at inline depth zero and retains its total hop count. No other boundary introduced by this change resets either budget. A service-identity context inherits both counters just as it currently inherits workflow correlation.

The existing cross-entity reaction depth limit remains 8. Caller headers do not set either counter. No Cedar policy, credential protocol, principal derivation, tenant selection, or HTTP authentication behavior changes.

This is a per-lineage bound. It does not claim aggregate accounting across fanout, durable budget persistence across restarts, or stronger loopback propagation than the current runtime provides. The application's declared turn and spending bounds continue to apply independently.

## State model and implementation correspondence

model.tla describes the counters and the only two budget-changing transitions: AdmitCallback increments both; Detach resets inline depth alone. model.cfg binds the production limits. The implementation and seeded tests must preserve the same transitions and bounds.

The budget is deliberately finite rather than configurable from an application request. A 40-turn research session needs approximately 240 callbacks; 512 provides room for setup, compaction, and completion while retaining a hard ceiling. The live test must establish that the actual app stays within this budget; that estimate is not a quality or liveness guarantee for every application.

## Failure behavior

Inline recursion still stops at the existing boundary. A background cycle stops once its lineage consumes 512 admitted callbacks. Refusal must not compensate newer state, authorize additional actions, or silently dispatch the rejected callback.

## Verification

A regression must fail against bd15e8920032fef5617bee6bde7729e411fdeb05 for a finite background workflow longer than eight callbacks. Production budget code runs under seeded schedules and identity transitions. Tests must also demonstrate inline and background cycle exhaustion, preserved hop count at detach, and unchanged reaction and authorization boundaries. A real detached callback path complements the state model and simulation.

After these checks, pin TemperPaw to the reviewed kernel commit, rebuild, and run the actual provider-backed Foresight flow. No production success is claimed by this kernel change alone.
