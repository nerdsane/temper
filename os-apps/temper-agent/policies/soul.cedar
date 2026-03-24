// AgentSoul — Cedar Authorization Policies

// Admins can do everything
permit(
  principal is Admin,
  action,
  resource is AgentSoul
);

// Supervisors and humans can create, publish, update, archive souls
permit(
  principal,
  action in [Action::"create", Action::"Create", Action::"Publish", Action::"Update", Action::"Archive"],
  resource is AgentSoul
) when {
  ["supervisor", "human"].contains(principal.agent_type)
};

// Any authenticated agent can read souls
permit(
  principal,
  action in [Action::"read", Action::"list"],
  resource is AgentSoul
);
