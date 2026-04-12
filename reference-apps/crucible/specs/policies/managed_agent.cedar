// Cedar policy stub — ManagedAgent
//
// Phase 1 of Crucible ships a permissive default: any principal can
// perform any action on a ManagedAgent. Real role-based policies land
// when the Session entity is introduced in Phase 2 (see ADR-0043).

permit(
    principal,
    action,
    resource is ManagedAgent
);
