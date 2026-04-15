// AgentSkill — Cedar Authorization Policies

// Admins can do everything
permit(
  principal is Admin,
  action,
  resource is AgentSkill
);

// Supervisors and humans can register, update, disable, enable skills
permit(
  principal,
  action in [Action::"create", Action::"Register", Action::"Update", Action::"Disable", Action::"Enable"],
  resource is AgentSkill
) when {
  ["supervisor", "human"].contains(principal.agent_type)
};

// Any authenticated agent can read skills
permit(
  principal,
  action in [Action::"read", Action::"list"],
  resource is AgentSkill
);
