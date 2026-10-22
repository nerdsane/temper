// Incident RCA — Postmortem Cedar Policies
//
// Agents draft; humans (or the supervisor) review and approve; publishing
// puts the analysis into the corpus future investigations must read.

// --- Universal: any agent can create, read, list postmortems ---

permit(
  principal,
  action in [Action::"create", Action::"read", Action::"list"],
  resource is Postmortem
);

// --- Drafting: agents ---

permit(
  principal,
  action in [Action::"AddRootCause", Action::"SubmitForReview"],
  resource is Postmortem
) when {
  ["agent", "supervisor", "human"].contains(principal.agent_type) &&
  context.agentTypeVerified == true
};

// --- Review & publish: supervisor or human ---

permit(
  principal,
  action in [Action::"RequestRevision", Action::"ApprovePostmortem", Action::"Publish"],
  resource is Postmortem
) when {
  ["supervisor", "human"].contains(principal.agent_type) &&
  context.agentTypeVerified == true
};
