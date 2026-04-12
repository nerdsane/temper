// Cedar policy stub — CallableAgent
//
// Permissive default. Real policies land with the multi-agent slice.

permit(
    principal,
    action,
    resource is CallableAgent
);
