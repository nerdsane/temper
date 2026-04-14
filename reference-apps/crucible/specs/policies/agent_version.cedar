// Cedar policy stub — AgentVersion
//
// Permissive default. The client-managed snapshot contract says rows
// should be append-only, but enforcing that via Cedar requires a real
// policy layer — landing with the Session slice (Phase 2).

permit(
    principal,
    action,
    resource is AgentVersion
);
