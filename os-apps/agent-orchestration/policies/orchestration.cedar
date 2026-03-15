// Agent Orchestration OS App Cedar policies.

// --- HeartbeatRun ---

permit(
  principal,
  action in [Action::"create", Action::"read", Action::"list"],
  resource is HeartbeatRun
);

permit(
  principal,
  action in [Action::"Schedule", Action::"ApproveBudget"],
  resource is HeartbeatRun
) when {
  ["supervisor", "human"].contains(principal.agent_type)
};

permit(
  principal,
  action in [Action::"CheckIn", Action::"StartExecution", Action::"RecordTurn", Action::"SaveCheckpoint", Action::"RecordResult", Action::"Complete"],
  resource is HeartbeatRun
) when {
  resource.AgentId == principal.id
};

permit(
  principal,
  action in [Action::"Fail", Action::"Cancel"],
  resource is HeartbeatRun
) when {
  resource.AgentId == principal.id || ["supervisor", "human"].contains(principal.agent_type)
};

// --- Organization ---

permit(
  principal,
  action in [Action::"read", Action::"list"],
  resource is Organization
);

permit(
  principal,
  action in [Action::"Configure", Action::"Activate", Action::"AddMember", Action::"RemoveMember", Action::"ResetBudgetCycle", Action::"Pause", Action::"Resume", Action::"Archive"],
  resource is Organization
) when {
  ["supervisor", "human"].contains(principal.agent_type)
};

permit(
  principal,
  action == Action::"RecordCost",
  resource is Organization
);

// --- BudgetLedger ---

permit(
  principal,
  action in [Action::"create", Action::"read", Action::"list", Action::"Record"],
  resource is BudgetLedger
);
