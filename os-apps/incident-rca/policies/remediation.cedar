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

// --- Approval: the human, holding the operator credential, only ---
// The deployment operator credential (TEMPER_API_KEY) is Ben's own key and
// resolves as verified agent_type "operator" — it IS the human gate in v1.
// Plain agents can never approve their own remediations.

permit(
  principal,
  action in [Action::"Approve", Action::"Reject"],
  resource is Remediation
) when {
  ["human", "operator"].contains(principal.agent_type) &&
  context.agentTypeVerified == true
};

// --- Execution: supervisor agent, operator, or human, after approval ---

permit(
  principal,
  action in [Action::"Execute", Action::"Succeed", Action::"Fail", Action::"Retry"],
  resource is Remediation
) when {
  ["supervisor", "human", "operator"].contains(principal.agent_type) &&
  context.agentTypeVerified == true
};
