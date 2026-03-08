// Project Management — Issue Cedar Policies
//
// Default-deny posture. Policies are generated from human approval decisions.
// This file provides initial bootstrap policies for common operations.

// Allow any agent to create and read issues
permit(
  principal,
  action in [Action::"create", Action::"read", Action::"list"],
  resource is Issue
);

// Allow assignee to take work actions on their issues
permit(
  principal,
  action in [Action::"StartWork", Action::"SubmitForReview"],
  resource is Issue
) when {
  resource.AssigneeId == principal.id
};
