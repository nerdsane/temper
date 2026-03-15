// Project Management — Issue Cedar Policies
//
// Human-in-the-loop model: agents act on human instruction.
// The planner CAN approve its own plan (because the human told it to).
// The real gate is the human saying "go ahead" in conversation.

// --- Universal: any agent can create, read, list, comment ---

permit(
  principal,
  action in [Action::"create", Action::"read", Action::"list", Action::"AddComment", Action::"AddLabel", Action::"SetDescription"],
  resource is Issue
);

// --- Triage & Prioritization: supervisors and humans ---

permit(
  principal,
  action in [Action::"MoveToTriage", Action::"MoveToTodo", Action::"SetPriority", Action::"Assign", Action::"AssignPlanner", Action::"Unassign", Action::"Cancel"],
  resource is Issue
) when {
  ["supervisor", "human"].contains(principal.agent_type)
};

// --- Planning: the assigned planner ---

permit(
  principal,
  action in [Action::"BeginPlanning", Action::"WritePlan"],
  resource is Issue
) when {
  resource.PlannerId == principal.id
};

// --- Plan Approval: supervisors and humans (planner CAN self-approve) ---

permit(
  principal,
  action in [Action::"ApprovePlan", Action::"RejectPlan", Action::"RevisePlan"],
  resource is Issue
) when {
  ["supervisor", "human"].contains(principal.agent_type)
};

// --- Implementation: the assigned implementer ---

permit(
  principal,
  action in [Action::"StartWork", Action::"UpdateImplementation", Action::"SubmitForReview"],
  resource is Issue
) when {
  resource.AssigneeId == principal.id
};

// --- Review: supervisors and humans ---

permit(
  principal,
  action in [Action::"ApproveReview", Action::"RequestChanges"],
  resource is Issue
) when {
  ["supervisor", "human"].contains(principal.agent_type)
};

// --- Reopen & Archive: supervisors and humans ---

permit(
  principal,
  action in [Action::"Reopen", Action::"Archive"],
  resource is Issue
) when {
  ["supervisor", "human"].contains(principal.agent_type)
};
