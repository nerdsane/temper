// Project Management — Issue Cedar Policies
//
// Default-deny posture. Agents can only act on issues assigned to them.
// Supervisors (haku, human) have broader permissions.

// Anyone can read and list issues
permit(
  principal,
  action in [Action::"read", Action::"list"],
  resource is Issue
);

// Anyone can create issues and add to backlog
permit(
  principal,
  action in [Action::"create", Action::"SetDescription", Action::"SetPriority",
             Action::"AddLabel", Action::"MoveToTriage", Action::"MoveToTodo",
             Action::"AddComment"],
  resource is Issue
);

// Supervisors (haku, human operators) can assign, cancel, approve, archive
permit(
  principal,
  action in [Action::"Assign", Action::"Unassign", Action::"Cancel",
             Action::"ApproveReview", Action::"Archive", Action::"Reopen",
             Action::"RequestChanges"],
  resource is Issue
) when {
  principal.agent_type == "haku" || principal.agent_type == "human"
};

// Assigned agent can start work, update implementation, submit for review
permit(
  principal,
  action in [Action::"StartWork", Action::"UpdateImplementation",
             Action::"SubmitForReview"],
  resource is Issue
) when {
  resource.AssigneeId == principal.id
};
