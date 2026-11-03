//! Cedar context keys only the platform sets on a record's moves and reads.
//!
//! A record's fields reach Cedar as `context.<field>` too: the engine merges
//! them in after the request's own keys, for domain policies. The platform
//! sets each key here on some requests about a record and not on others, so a
//! record field of that name would stand in for it on the others: a user who
//! can write the field would pass a policy meant only for the platform's own
//! moves. A key on this list is never taken from a record, and each key names
//! that attack.
//!
//! The keys the WASM gate sets only on a module's host calls (`tenant`,
//! `entityType`, `triggerAction`, `method`, `url`) are not here. A host call's
//! resource is never a record and the gate always sets them, so no field
//! stands in for one; on a record's move those names are the record's own
//! fields, which domain policies may read.

/// The caller's session, from their token. A field `sessionId` would pass a
/// policy that ties a move to one session.
pub const SESSION_ID: &str = "sessionId";

/// The calling agent, from its token. A field `agentId` would pass a policy
/// for one agent, for anyone.
pub const AGENT_ID: &str = "agentId";

/// The calling agent's type. A field `agentType` would pass a policy for one
/// kind of agent, for any caller.
pub const AGENT_TYPE: &str = "agentType";

/// Whether the platform verified the agent's type. A field set to `true`
/// would make a self-declared agent type trusted.
pub const AGENT_TYPE_VERIFIED: &str = "agentTypeVerified";

/// The caller's role, from their token. A field `role` would pass a role
/// check, such as an admin's, for any caller.
pub const ROLE: &str = "role";

/// The user an agent acts for, from its token. A field `actingFor` would let
/// any caller act for the user it names.
pub const ACTING_FOR: &str = "actingFor";

/// The `[[carry]]` move whose `then` move the platform is making. A field
/// `carryMove` would pass a policy that lets only the carry service's then
/// move through, for a user's own move on that record.
pub const CARRY_MOVE: &str = "carryMove";

/// The WASM module making a move through its own tdata host, or a host call.
/// A field `module` would pass `context.module == "steps"`, meant for the
/// steps module's moves, for a user's own move on that record.
pub const MODULE: &str = "module";

/// The trigger rule whose declared principal is making a reaction move. A
/// field `triggerRule` would pass a policy that lets only that rule's
/// reaction make the move, for a user's own move.
pub const TRIGGER_RULE: &str = "triggerRule";

/// The entity type whose move fired the trigger. A field would pass a policy
/// that lets only reactions to that type make the move.
pub const TRIGGER_SOURCE_ENTITY_TYPE: &str = "triggerSourceEntityType";

/// The record whose move fired the trigger. A field would pass a policy that
/// lets only reactions to that record make the move.
pub const TRIGGER_SOURCE_ENTITY_ID: &str = "triggerSourceEntityId";

/// The move that fired the trigger. A field would pass a policy that lets
/// only reactions to that move make the move.
pub const TRIGGER_SOURCE_ACTION: &str = "triggerSourceAction";

/// That a trigger's declared principal is making the move. A field set to
/// `true` would pass a policy for any declared principal, for a user's own
/// move.
pub const TRIGGER_DECLARED_PRINCIPAL: &str = "triggerDeclaredPrincipal";

/// That a reaction's caller was rebuilt from bare agent ids, not a resolved
/// identity. A field would mark a user's own move as such a reaction, or
/// hide that a reaction is one from a policy that refuses them.
pub const TRIGGER_INHERITED_CONTEXT_APPROXIMATE: &str = "triggerInheritedContextApproximate";

// How the move came about (`MoveCause`). The engine sets `via` and `depth` on
// every check and the rest on a reaction's or a timer's, from the security
// context's cause alone: neither a record field nor a context attribute
// speaks under these names, so only the kernel's own reaction and timer paths
// set them.

/// `"request"`, `"reaction"` or `"timer"`. A field `via` set to `"reaction"`
/// would pass an app's hook forbid narrowed to direct requests, for any move
/// the hook picks.
pub const VIA: &str = "via";

/// Reaction hops since the chain's request or timer fire. A field would pass
/// a policy that admits only a chain's later hops, for a direct request.
pub const DEPTH: &str = "depth";

/// The rule making a reaction, `<Type>:<Action>:<trigger>`. A field would
/// pass a policy that lets only that rule's reaction make the move, for a
/// caller's own move.
pub const RULE: &str = "rule";

/// The move that set a reaction off. A field would pass a policy that admits
/// only reactions to that move.
pub const CAUSE_ACTION: &str = "causeAction";

/// The entity type of the move that set a reaction off. A field would pass a
/// policy that admits only reactions to that type's moves.
pub const CAUSE_ENTITY_TYPE: &str = "causeEntityType";

/// The record whose move set a reaction off. A field would pass a policy
/// that admits only reactions to that record's moves.
pub const CAUSE_ENTITY_ID: &str = "causeEntityId";

/// Who armed the timer a chain started from. A field would pass a policy
/// that admits only the timers one principal armed, for anyone's request.
pub const ARMED_BY: &str = "armedBy";

/// The keys of a move's cause, which only the engine sets, from the cause.
pub const CAUSE: [&str; 7] = [
    VIA,
    DEPTH,
    RULE,
    CAUSE_ACTION,
    CAUSE_ENTITY_TYPE,
    CAUSE_ENTITY_ID,
    ARMED_BY,
];

/// Every key above.
pub const ALL: [&str; 21] = [
    SESSION_ID,
    AGENT_ID,
    AGENT_TYPE,
    AGENT_TYPE_VERIFIED,
    ROLE,
    ACTING_FOR,
    CARRY_MOVE,
    MODULE,
    TRIGGER_RULE,
    TRIGGER_SOURCE_ENTITY_TYPE,
    TRIGGER_SOURCE_ENTITY_ID,
    TRIGGER_SOURCE_ACTION,
    TRIGGER_DECLARED_PRINCIPAL,
    TRIGGER_INHERITED_CONTEXT_APPROXIMATE,
    VIA,
    DEPTH,
    RULE,
    CAUSE_ACTION,
    CAUSE_ENTITY_TYPE,
    CAUSE_ENTITY_ID,
    ARMED_BY,
];
