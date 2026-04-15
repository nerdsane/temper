// Cedar policy stub — SessionResource
//
// Phase 2 of Crucible ships a permissive default: any principal can
// perform any action on a SessionResource. Real role-based policies
// are deferred to a follow-up slice (see ADR-0044).

permit(
    principal,
    action,
    resource is SessionResource
);
