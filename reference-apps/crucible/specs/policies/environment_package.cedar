// Cedar policy stub — EnvironmentPackage
//
// Phase 0 of Crucible ships a permissive default. Real role-based
// policies land with the Session entity in Phase 2 (see ADR-0042).

permit(
    principal,
    action,
    resource is EnvironmentPackage
);
