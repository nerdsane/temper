// Cedar policy stub — Session
//
// Phase 2 of Crucible ships a permissive default: any principal can
// perform any action on a Session. Real role-based policies (planner vs
// implementer vs supervisor) are deferred to a follow-up slice paired
// with the PM-app-as-Cedar-worked-example chapter (see ADR-0044).

permit(
    principal,
    action,
    resource is Session
);
