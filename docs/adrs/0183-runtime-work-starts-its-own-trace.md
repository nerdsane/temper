# ADR-0183: Runtime Work Starts Its Own Trace

- Status: Accepted
- Date: 2026-10-24
- Deciders: Temper core maintainers
- Supersedes: ADR-0084 (long-lived workflow root spans), ADR-0101 (workflow root drain attribution)
- Related:
  - ADR-0098: Background WASM Trace Retention (the `dispatch.scheduled_actions` reduction is removed)
  - `crates/temper-server/src/trace_context.rs`
  - `crates/temper-server/src/state/dispatch/actions.rs`
  - `crates/temper-server/src/api/decisions.rs`
  - `crates/temper-platform/src/hooks/governance_callback.rs`

## Context

ADR-0084 kept a `{RootEntity}.workflow` root span open in process memory for a
workflow run and put every dispatch of that run under it, closing it only when
the root record reached `Completed`, `Failed`, `Cancelled` or `Terminated`.

Dispatch writes `workflow.run_id` into the agent context it hands to its
effects, so every state timer, scheduled action, spawn, background reaction and
integration callback looked like an explicit workflow and opened, or joined,
such a root. A span is exported when it ends, so:

- the root was never exported for a record whose spec has none of those four
  status names, and Datadog showed a trace with no root;
- the trace kept every later runtime-started move of the record, so it grew for
  as long as the record moved (one record held 2,175 spans over about nine
  minutes).

Scheduled actions and spawned records also ran as children of the request that
caused them, so a request's trace lasted as long as its delayed work.

## Decision

Work the runtime starts on its own, after the move that caused it, is a trace of
its own. Its first span is opened with `trace_context::runtime_work_span!`: the
span has no parent and one span link to the span of the move that handed the
work off (`trace_context::cause()`). It applies to every kind: state timers,
scheduled actions, spawned records, background reactions, background WASM and
adapter integrations (with the callbacks they dispatch), integration failure
compensation, the GovernanceDecision move that an approve or deny request hands
off without waiting for it (linked to that request), and the platform's
governance callback that resumes or fails the waiting record (linked to the
decision's move). The macro is exported (`temper_server::runtime_work_span!`)
so work handed off outside `temper-server` uses the same rule. Every root it
opens carries `runtime_work = "<kind>"` (a required argument: `state_timeout`,
`scheduled_action`, `background_spawn`, `reaction`, `wasm_integration`,
`adapter_integration`, `integration_compensation`, `decision_resolution`,
`governance_callback`), so a trace pipeline keeps runtime work with
`parent_id:0 @runtime_work:*` and a new kind needs no rule change. A delayed piece
of work opens its span when the wait ends, so the span's duration is the
work's.

No span is kept open across moves. `WorkflowSpanRegistry`, its hardcoded
agent-cycle entity names and terminal status names, and the drain grace are
removed. Dispatch runs in the span it is called in and never re-parents or
re-links itself from ids carried in the agent context. `workflow.run_id`,
`workflow.root_entity_type` and `workflow.root_entity_id` stay on every dispatch
span, so a whole run is still one query, and the links walk it move by move.

The reduced sampling of `dispatch.scheduled_actions` (ADR-0098) is removed: a
scheduled action is a move like a timer's, and every runtime-started move keeps
its trace. `dispatch.phase.query_projection` stays reduced.

**Why this approach**: a trace that stays open until a status name appears is a
trace that can stay open forever. One rule at the point where work detaches
covers every kind of runtime work, needs no list of entity or status names, and
makes each trace end when its work ends.

## Consequences

- An agent session is no longer one long trace. Its moves are short traces
  joined by `workflow.run_id` and by links from each piece of work to its cause.
- Bookkeeping a move queues (trajectory outboxes, snapshot and projection
  queues, webhooks) still continues the move's trace, as before.

## Rollback Policy

Revert the change. No state is persisted; traces before and after are
independent.
