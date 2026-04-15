// AgentMemory — Cedar Authorization Policies

// Admins can do everything
permit(
  principal is Admin,
  action,
  resource is AgentMemory
);

// System, supervisor, and human principals can manage memories
permit(
  principal,
  action in [Action::"create", Action::"Save", Action::"Update", Action::"Recall"],
  resource is AgentMemory
) when {
  ["system", "supervisor", "human"].contains(principal.agent_type)
};

// Agents can save, update, and recall memories scoped to their own soul_id
permit(
  principal,
  action in [Action::"create", Action::"Save", Action::"Update", Action::"Recall"],
  resource is AgentMemory
) when {
  principal.agent_type == "agent" && resource.SoulId == principal.soul_id
};

// Supervisors and humans can archive any memory
permit(
  principal,
  action in [Action::"Archive"],
  resource is AgentMemory
) when {
  ["supervisor", "human"].contains(principal.agent_type)
};

// Any authenticated agent can read/list memories
permit(
  principal,
  action in [Action::"read", Action::"list"],
  resource is AgentMemory
);
