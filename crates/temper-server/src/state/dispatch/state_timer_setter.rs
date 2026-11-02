//! Who a state timer fires as when nothing saved names its setter.
//!
//! A timer fires as whoever set it, and its row keeps their whole identity.
//! A record can still be in a timed state with nothing that names anyone: it
//! entered the state before timer rows were kept, or its row was lost. Such a
//! timer is armed as the kernel's [`STATE_TIMER_SERVICE`], which acts for no
//! one, and a timer whose own move resets it keeps firing as the service from
//! then on, since each fire arms the next as the fire's caller. Every move
//! after it that a person's standing permits is refused: a reaction Cedar
//! checks as the service, and a move that binds its caller
//! (`authenticated_subject`).
//!
//! The record's journal keeps who each move ran as (`EventOrigin`), however it
//! came about, and a move the kernel's own identities made names no one. The
//! newest move that names someone says who the record's later timers follow
//! from. A timer that nothing else names a setter for is set as that
//! principal; only when the journal names no one does it stay the service's.
//!
//! The journal keeps less than a row. A person comes back as they moved the
//! record. An agent comes back only when its event keeps whom it acted for,
//! its type and its role: Cedar's role forbids decide what an agent may do,
//! so an agent set up without its role could do more than its token let it.
//! An agent whose event does not keep them (one written before events kept
//! them) leaves the timer as it was. A move from before moves named who they
//! ran as is not read: a record only such moves made stays nameless. Neither keeps the credential its token
//! was issued under, so the fire's revocation check does not apply to it, as
//! for a timer set before identities kept credentials (`timer_standing`).

use temper_authz::{PrincipalKind, SecurityContext};
use temper_runtime::persistence::PersistenceError;
use temper_runtime::tenant::TenantId;

use crate::entity_actor::{EntityEvent, EventOrigin};
use crate::request_context::AgentContext;
use crate::state::ServerState;

/// The service a timer fires as when nobody armed it: a record found in a
/// timed state with no timer armed, no row and no move that names someone.
pub(super) const STATE_TIMER_SERVICE: &str = "state-timer";

/// How many journal events one read takes while looking back for the newest
/// move that names someone. The reason is the store: a read costs its rows,
/// and most records' newest such move is among their newest events. A longer
/// journal is read page by page, as far back as it goes.
const JOURNAL_PAGE: usize = 64;

/// Whether `agent` is the [`STATE_TIMER_SERVICE`]: a timer armed for no one.
pub(super) fn is_state_timer_service(agent: &AgentContext) -> bool {
    agent.security_ctx.as_ref().is_some_and(|context| {
        context.principal.role.as_deref() == Some("service")
            && context.principal.id == format!("service:{STATE_TIMER_SERVICE}")
    })
}

impl ServerState {
    /// The principal of the record's newest move that names someone, set up
    /// again as its journal keeps it; None when the journal names no one it
    /// can set up (see the module doc), or there is no journal. An error is a
    /// journal that could not be read.
    pub(super) async fn journal_setter(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
    ) -> Result<Option<AgentContext>, PersistenceError> {
        let Some((store, _backend)) = self.event_journal() else {
            return Ok(None);
        };
        let persistence_id = format!("{tenant}:{entity_type}:{entity_id}");
        let mut before = None;
        loop {
            let page = store
                .read_events_page(&persistence_id, before, JOURNAL_PAGE)
                .await?;
            for envelope in page.iter().rev() {
                let Ok(event) = serde_json::from_value::<EntityEvent>(envelope.payload.clone())
                else {
                    continue;
                };
                if event.origin.by.is_some() {
                    return Ok(named_by(&event.origin));
                }
            }
            match page.first() {
                Some(oldest) if page.len() == JOURNAL_PAGE && oldest.sequence_nr > 1 => {
                    before = Some(oldest.sequence_nr);
                }
                _ => return Ok(None),
            }
        }
    }
}

/// The principal a move's origin names, as a context to fire with; None
/// for an agent whose origin does not keep whom it acted for, its type and
/// its role.
fn named_by(origin: &EventOrigin) -> Option<AgentContext> {
    let by = origin.by.as_deref().filter(|by| !by.is_empty())?;
    let security_ctx = match (&origin.agent_type, &origin.acting_for) {
        (None, None) => SecurityContext::from_verified_jwt(
            by,
            PrincipalKind::Customer,
            None,
            None,
            origin.role.as_deref(),
            None,
        ),
        (Some(agent_type), Some(acting_for)) => SecurityContext::from_verified_jwt(
            by,
            PrincipalKind::Agent,
            Some(agent_type),
            Some(acting_for),
            Some(origin.role.as_deref()?),
            None,
        )
        .with_label(origin.label.as_deref()),
        _ => return None,
    };
    Some(AgentContext {
        security_ctx: Some(security_ctx),
        agent_id: Some(by.to_string()),
        agent_type: origin.agent_type.clone(),
        ..AgentContext::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity_actor::EventVia;

    fn origin(by: &str) -> EventOrigin {
        EventOrigin {
            via: Some(EventVia::Request),
            by: Some(by.into()),
            ..EventOrigin::default()
        }
    }

    #[test]
    fn a_person_comes_back_as_they_moved_the_record() {
        let person = EventOrigin {
            role: Some("owner".into()),
            ..origin("ann")
        };
        let context = named_by(&person).and_then(|agent| agent.security_ctx);
        let principal = context.expect("a person comes back").principal;
        assert_eq!(principal.id, "ann");
        assert_eq!(principal.kind, PrincipalKind::Customer);
        assert_eq!(principal.role.as_deref(), Some("owner"));
        assert_eq!(principal.acting_for, None);
    }

    #[test]
    fn an_agent_comes_back_only_with_its_role() {
        let agent = EventOrigin {
            acting_for: Some("ann".into()),
            agent_type: Some("claude-code".into()),
            ..origin("hub-client")
        };
        assert!(named_by(&agent).is_none(), "no role, no agent");
        let with_role = EventOrigin {
            role: Some("run".into()),
            ..agent
        };
        let principal = named_by(&with_role)
            .and_then(|agent| agent.security_ctx)
            .expect("an agent with its role comes back")
            .principal;
        assert_eq!(principal.kind, PrincipalKind::Agent);
        assert_eq!(principal.acting_for.as_deref(), Some("ann"));
        assert_eq!(principal.agent_type.as_deref(), Some("claude-code"));
        assert_eq!(principal.role.as_deref(), Some("run"));
    }

    #[test]
    fn only_the_state_timer_service_is_a_timer_for_no_one() {
        assert!(is_state_timer_service(&AgentContext::for_service(
            STATE_TIMER_SERVICE
        )));
        assert!(!is_state_timer_service(&AgentContext::for_service("other")));
        assert!(!is_state_timer_service(&AgentContext::system()));
        let person = named_by(&origin("ann")).expect("a person");
        assert!(!is_state_timer_service(&person));
    }
}
