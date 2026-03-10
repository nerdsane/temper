// Project Management — Issue Cedar Policies
//
// Role separation: planners plan, supervisors approve, implementers execute.
// No agent can both plan and approve the same issue.

// --- Universal: any agent can create, read, list, comment ---

permit(
  principal,
  action in [Action::"create", Action::"read", Action::"list", Action::"AddComment", Action::"AddLabel"],
  resource is Issue
);

// --- Triage & Prioritization: supervisors and humans ---

permit(
  principal,
  action in [Action::"MoveToTriage", Action::"MoveToTodo", Action::"SetPriority", Action::"Assign", Action::"AssignPlanner", Action::"Unassign", Action::"Cancel"],
  resource is Issue
) when {
  principal.agent_type in ["supervisor", "human"]
};

// --- Planning: only the assigned planner ---

permit(
  principal,
  action in [Action::"BeginPlanning", Action::"WritePlan"],
  resource is Issue
) when {
  resource.PlannerId == principal.id
};

// --- Plan Approval: supervisors and humans only, NOT the planner ---

permit(
  principal,
  action in [Action::"ApprovePlan", Action::"RejectPlan", Action::"RevisePlan"],
  resource is Issue
) when {
  principal.agent_type in ["supervisor", "human"] &&
  resource.PlannerId != principal.id
};

// --- Implementation: only the assigned implementer ---

permit(
  principal,
  action in [Action::"StartWork", Action::"UpdateImplementation", Action::"SubmitForReview"],
  resource is Issue
) when {
  resource.AssigneeId == principal.id
};

// --- Review: supervisors and humans, NOT the implementer ---

permit(
  principal,
  action in [Action::"ApproveReview", Action::"RequestChanges"],
  resource is Issue
) when {
  principal.agent_type in ["supervisor", "human"] &&
  resource.AssigneeId != principal.id
};

// --- Reopen & Archive: supervisors and humans ---

permit(
  principal,
  action in [Action::"Reopen", Action::"Archive"],
  resource is Issue
) when {
  principal.agent_type in ["supervisor", "human"]
};
