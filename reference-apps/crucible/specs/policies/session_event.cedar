// Cedar policy stub — SessionEvent
//
// Phase 2 of Crucible ships a permissive default: any principal can
// POST a SessionEvent. Events are conceptually append-only; a real
// policy set would permit Create but deny Update on this resource
// type. Phase 2 stubs both for now, matching AgentVersion (see
// ADR-0044).

permit(
    principal,
    action,
    resource is SessionEvent
);
