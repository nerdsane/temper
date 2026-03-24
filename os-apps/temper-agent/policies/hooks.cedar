// ToolHook — Cedar Authorization Policies

// Admins can do everything
permit(
  principal is Admin,
  action,
  resource is ToolHook
);

// Supervisors and humans can manage hooks
permit(
  principal,
  action in [Action::"create", Action::"Register", Action::"Disable", Action::"Enable"],
  resource is ToolHook
) when {
  ["supervisor", "human"].contains(principal.agent_type)
};

// Any authenticated agent can read hooks (tool_runner queries them)
permit(
  principal,
  action in [Action::"read", Action::"list"],
  resource is ToolHook
);
