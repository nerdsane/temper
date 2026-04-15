permit(
  principal is Admin,
  action,
  resource is IntentDiscovery
);

permit(
  principal,
  action in [Action::"create", Action::"read", Action::"list", Action::"Trigger"],
  resource is IntentDiscovery
) when {
  ["system", "supervisor", "human"].contains(principal.agent_type)
};

permit(
  principal,
  action in [Action::"GatheringComplete", Action::"AnalysisComplete", Action::"ProposalComplete", Action::"Fail"],
  resource is IntentDiscovery
) when {
  principal.agent_type == "system"
};

permit(
  principal is Agent,
  action == Action::"http_call",
  resource is HttpEndpoint
) when {
  ["gather_signals", "spawn_analyst", "create_proposals"].contains(context.module)
};
