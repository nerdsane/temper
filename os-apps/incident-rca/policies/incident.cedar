// Incident RCA — Incident Cedar Policies
//
// Roles: agents investigate and document; the supervisor agent (Astro)
// drives the workflow; only a verified human can approve a Remediation
// (see remediation.cedar). The state machine guards in incident.ioa.toml
// (prior-review + hypothesis before FixProposed, verification before
// Closed) are the compounding rules; these policies govern WHO may act.

// --- Universal: any agent can create, read, list incidents ---

permit(
  principal,
  action in [Action::"create", Action::"read", Action::"list"],
  resource is Incident
);

// --- Investigation & documentation: agents, supervisor, operator ---

permit(
  principal,
  action in [Action::"BeginInvestigation", Action::"AttachEvidence", Action::"RecordPriorReview", Action::"CompleteDocumentation", Action::"StateHypothesis", Action::"ProposeFix", Action::"BeginVerification", Action::"RecordVerification", Action::"Close"],
  resource is Incident
) when {
  ["agent", "supervisor", "human", "operator"].contains(principal.agent_type) &&
  context.agentTypeVerified == true
};

// --- Reopen: any agent (incl. the sensor identity) may reopen on recurrence ---

permit(
  principal,
  action in [Action::"Reopen"],
  resource is Incident
) when {
  ["agent", "supervisor", "human", "system"].contains(principal.agent_type) &&
  context.agentTypeVerified == true
};

// --- Observe: supervisors and humans can read observe/evolution data ---

permit(
  principal,
  action in [Action::"read_evolution", Action::"read_observe", Action::"read_trajectories"],
  resource
) when {
  ["supervisor", "human"].contains(principal.agent_type) &&
  context.agentTypeVerified == true
};
