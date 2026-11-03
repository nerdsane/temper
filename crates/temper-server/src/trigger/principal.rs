//! Who a reaction runs as, how its move came about, and the idempotency key
//! it carries.
//!
//! A reaction runs as the invoking principal, or as the service its rule
//! declares (ADR-0046). Either way the security context it is checked with
//! carries a [`MoveCause`]: the rule, the move that set it off, the chain's
//! depth and, in a timer's chain, the timer's arm. Cedar sees it as
//! `context.via` and the rest.

use temper_authz::{MoveCause, ReactionHop, SecurityContext, platform_keys};

use crate::idempotency::IdempotencyKey;
use crate::request_context::AgentContext;

/// The cause of the reaction `rule_name` makes when `source_action` on
/// `source_entity_type`/`source_entity_id`, made with `invoking_ctx`, sets it
/// off: one hop deeper than that move, in the same timer's chain if any.
fn reaction_cause(
    rule_name: &str,
    source_entity_type: &str,
    source_entity_id: &str,
    source_action: &str,
    invoking_ctx: &AgentContext,
) -> MoveCause {
    MoveCause::reaction(
        ReactionHop {
            rule: rule_name.to_string(),
            action: source_action.to_string(),
            entity_type: source_entity_type.to_string(),
            entity_id: source_entity_id.to_string(),
        },
        invoking_ctx
            .security_ctx
            .as_ref()
            .and_then(|context| context.cause.as_ref()),
    )
}

/// The service that makes a reaction for the user its rule's `as` field
/// names: Cedar sees `Agent::"service:reaction"` acting for that user.
const ACTS_FOR_SERVICE: &str = "reaction";

/// Who a reaction is made as, from its rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ReactionMaker<'a> {
    /// Whoever made the source move (the rule names no one).
    Inherit,
    /// The service the rule's `principal` declares (ADR-0046).
    Service(&'a str),
    /// The kernel acting for this user, whom the source record's `as` field
    /// names: a field only that user's own identity writes.
    For(&'a str),
}

/// ADR-0046: resolve the dispatch context for a reaction.
///
/// - When the rule declares a principal (`Service(service_name)`), build a
///   synthetic `AgentContext` identifying the named service. Cedar policies
///   can match on `principal.role`, `principal.agent_type`, or
///   `principal.id == Service::"<name>"` — whichever style the tenant
///   prefers. The `agent_type` slot carries the name so the Cedar request
///   sees it via `principal.agent_type == "<name>"`.
/// - When the rule's `as` names a user (`For(user)`), the reaction is made by
///   the kernel acting for that user: `Agent::"service:reaction"` with
///   `actingFor` the user and no service role, so Cedar judges it, and the
///   moves it sets off, as acting for them, and a parameter bound to the
///   caller's identity binds theirs. Whoever made the source move chose only
///   that the reaction happens, which Cedar let them; the user is the one who
///   put their own name on the record.
/// - When the rule names no one (`Inherit`), inherit the invoking context
///   directly — the trigger runs as whoever called the source action.
///
/// Either way the reaction carries its own idempotency key, derived from the
/// caller's, the rule's name and the reaction's `depth` in its chain
/// ([`reaction_idempotency_key`]), and its security context carries the
/// reaction's [`reaction_cause`], so the moves it sets off in turn are one hop
/// deeper. The cause is returned too: a context without a security context is
/// checked with one approximated from it, which must carry the same cause
/// ([`effective_trigger_security_context`]).
pub(super) fn resolve_trigger_principal(
    maker: ReactionMaker<'_>,
    invoking_ctx: &AgentContext,
    rule_name: &str,
    source_entity_type: &str,
    source_entity_id: &str,
    source_action: &str,
    depth: u32,
) -> (AgentContext, MoveCause) {
    let cause = reaction_cause(
        rule_name,
        source_entity_type,
        source_entity_id,
        source_action,
        invoking_ctx,
    );
    let mut ctx = match maker {
        ReactionMaker::For(user) => {
            let mut ctx = AgentContext::for_service_inheriting(ACTS_FOR_SERVICE, invoking_ctx);
            if let Some(security_ctx) = ctx.security_ctx.as_mut() {
                security_ctx.principal.acting_for = Some(user.to_string());
                // Acting for a user, the service has a subject (as a carry's
                // then move does), so it is no bare kernel identity.
                security_ctx.principal.role = None;
            }
            ctx
        }
        ReactionMaker::Service(service_name) if !service_name.is_empty() => {
            let mut ctx = AgentContext::for_service_inheriting(service_name, invoking_ctx);
            if let Some(security_ctx) = ctx.security_ctx.as_mut() {
                security_ctx.context_attrs.insert(
                    platform_keys::TRIGGER_RULE.to_string(),
                    serde_json::Value::String(rule_name.to_string()),
                );
                security_ctx.context_attrs.insert(
                    platform_keys::TRIGGER_SOURCE_ENTITY_TYPE.to_string(),
                    serde_json::Value::String(source_entity_type.to_string()),
                );
                security_ctx.context_attrs.insert(
                    platform_keys::TRIGGER_SOURCE_ENTITY_ID.to_string(),
                    serde_json::Value::String(source_entity_id.to_string()),
                );
                security_ctx.context_attrs.insert(
                    platform_keys::TRIGGER_SOURCE_ACTION.to_string(),
                    serde_json::Value::String(source_action.to_string()),
                );
                security_ctx.context_attrs.insert(
                    platform_keys::TRIGGER_DECLARED_PRINCIPAL.to_string(),
                    serde_json::Value::Bool(true),
                );
            }
            ctx
        }
        _ => invoking_ctx.clone(),
    };
    if let Some(security_ctx) = ctx.security_ctx.as_mut() {
        security_ctx.cause = Some(cause.clone());
    }
    ctx.idempotency_key = reaction_idempotency_key(invoking_ctx, rule_name, depth);
    (ctx, cause)
}

/// Reactions a chain ran before chains stopped at their first repeated step:
/// earlier servers stopped every chain after eight. Their keys are stored,
/// and a key here is compared by its text, so these levels keep the form
/// they were written in.
const LEVELS_KEYED_BY_SUFFIX: u32 = 8;

/// A reaction's idempotency key: the caller's, made the rule's own (ADR-0048).
/// A retry of the source action carries the same caller key, so its reactions
/// are deduplicated too. The caller's key itself would make a reaction on the
/// source's own record (`same_id`) a duplicate of the source action, which
/// that record has just recorded under the key, and the reaction would be
/// answered as done without running.
///
/// The first [`LEVELS_KEYED_BY_SUFFIX`] levels append `:reaction:<rule>` to
/// the caller's key, as earlier servers did: a source action retried after an
/// upgrade sends its reactions again, and only the key they were stored under
/// shows them done. Deeper levels, which no earlier server ran, end in a
/// digest of the caller's key and the rule instead, so a key stops growing: a
/// chain is as long as its data, every event it writes stores its key, and a
/// key that grew at every level would make a chain's keys cost the square of
/// its length.
fn reaction_idempotency_key(
    invoking_ctx: &AgentContext,
    rule_name: &str,
    depth: u32,
) -> Option<IdempotencyKey> {
    use sha2::{Digest, Sha256};
    invoking_ctx.idempotency_key.as_ref().map(|key| {
        let key = key.as_str();
        IdempotencyKey::FollowUp(if depth < LEVELS_KEYED_BY_SUFFIX {
            format!("{key}:reaction:{rule_name}")
        } else {
            let digest = Sha256::digest(format!("{key}\0{rule_name}").as_bytes());
            format!("reaction:{rule_name}:{digest:x}")
        })
    })
}

/// The security context a reaction is checked with: its dispatch context's
/// own, which carries `cause`, or one approximated from the dispatch
/// context's agent fields when it has none, with `cause` set on it.
pub(super) fn effective_trigger_security_context(
    agent_ctx: &AgentContext,
    cause: &MoveCause,
) -> SecurityContext {
    if let Some(security_ctx) = &agent_ctx.security_ctx {
        return security_ctx.clone();
    }

    let mut security_ctx = SecurityContext::anonymous().with_agent_context(
        agent_ctx.agent_id.as_deref(),
        agent_ctx.session_id.as_deref(),
        agent_ctx.agent_type.as_deref(),
    );
    security_ctx.context_attrs.insert(
        platform_keys::TRIGGER_INHERITED_CONTEXT_APPROXIMATE.to_string(),
        serde_json::Value::Bool(true),
    );
    security_ctx.cause = Some(cause.clone());
    security_ctx
}
