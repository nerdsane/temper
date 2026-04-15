// Evolution Skill Cedar Policies
//
// Governs the autonomy slider for spec evolution: who can start, approve,
// reject, and deploy evolution candidates.

// --- EvolutionRun GEPA pipeline actions ---
// Anyone can start an evolution run.
permit(principal, action == Action::"Start", resource is EvolutionRun);

// Pipeline actions are system/agent-driven — permit for all principals.
permit(principal, action == Action::"SelectCandidate", resource is EvolutionRun);
permit(principal, action == Action::"RecordEvaluation", resource is EvolutionRun);
permit(principal, action == Action::"RecordDataset", resource is EvolutionRun);
permit(principal, action == Action::"RecordMutation", resource is EvolutionRun);
permit(principal, action == Action::"RecordVerificationPass", resource is EvolutionRun);
permit(principal, action == Action::"RecordVerificationFailure", resource is EvolutionRun);
permit(principal, action == Action::"ExhaustRetries", resource is EvolutionRun);
permit(principal, action == Action::"RecordScore", resource is EvolutionRun);
permit(principal, action == Action::"RecordFrontier", resource is EvolutionRun);
permit(principal, action == Action::"RecordFrontierAutoApprove", resource is EvolutionRun);
permit(principal, action == Action::"ContinueEvolution", resource is EvolutionRun);
permit(principal, action == Action::"Fail", resource is EvolutionRun);

// Read/list/create are open.
permit(principal, action in [Action::"create", Action::"read", Action::"list"], resource is EvolutionRun);
permit(principal, action in [Action::"create", Action::"read", Action::"list"], resource is SentinelMonitor);
permit(principal == Agent::"gepa-proposer-agent", action == Action::"write_trajectories", resource is Trajectory);

// Only humans can approve in full-human mode (default).
permit(principal, action == Action::"Approve", resource is EvolutionRun)
  when { principal.type == "Human" };

// Verified agents can approve low-risk mutations in supervised mode.
permit(principal, action == Action::"Approve", resource is EvolutionRun)
  when { principal.agentTypeVerified == true && resource.autonomy_level == "auto" };

// CRITICAL: Self-approval prohibition — proposer cannot approve own mutation.
forbid(principal, action == Action::"Approve", resource is EvolutionRun)
  when { resource.proposer_agent_id == principal.id };

// Anyone can reject a candidate.
permit(principal, action == Action::"Reject", resource is EvolutionRun);

// Only the system or verified agents can deploy.
permit(principal, action == Action::"Deploy", resource is EvolutionRun)
  when { principal.agentTypeVerified == true };

// Sentinel monitoring — anyone can trigger checks.
permit(principal, action == Action::"CheckSentinel", resource is SentinelMonitor);
permit(principal, action == Action::"AlertsFound", resource is SentinelMonitor);
permit(principal, action == Action::"NoAlerts", resource is SentinelMonitor);
permit(principal, action == Action::"CreateEvolutionRun", resource is SentinelMonitor);

// GEPA proposer module orchestrates TemperAgent over local Temper API.
permit(
  principal is Agent,
  action == Action::"http_call",
  resource is HttpEndpoint
) when {
  ["gepa-proposer-agent", "gepa-verify"].contains(context.module) &&
  ["127.0.0.1", "localhost"].contains(context.domain)
};

permit(
  principal is Agent,
  action == Action::"access_secret",
  resource is Secret
) when {
  ["gepa-proposer-agent", "gepa-verify"].contains(context.module)
};
