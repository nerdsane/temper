// Cedar policy stub — Environment
//
// Phase 0 of Crucible ships a permissive default: any principal can
// perform any action on an Environment. Real role-based policies land
// when the Session entity is introduced in Phase 2 (see ADR-0042).

permit(
    principal,
    action,
    resource is Environment
);
