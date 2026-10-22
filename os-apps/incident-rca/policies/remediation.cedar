// Incident RCA — Remediation Cedar Policies
//
// The human gate: Approve and Reject are HUMAN-ONLY in v1. There is no
// auto-approve path in the spec or in policy. Agents propose and (after
// approval) execute; the human decides.

// --- Universal: any agent can create, read, list remediations ---

permit(
  principal,
  action in [Action::"create", Action::"read", Action::"list"],
  resource is Remediation
);

// --- Approval: humans only ---

permit(
  principal,
  action in [Action::"Approve", Action::"Reject"],
  resource is Remediation
) when {
  principal.agent_type == "human" &&
  context.agentTypeVerified == true
};

// --- Execution: supervisor agent or human, after approval ---

permit(
  principal,
  action in [Action::"Execute", Action::"Succeed", Action::"Fail", Action::"Retry"],
  resource is Remediation
) when {
  ["supervisor", "human"].contains(principal.agent_type) &&
  context.agentTypeVerified == true
};
