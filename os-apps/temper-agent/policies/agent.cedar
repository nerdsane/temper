// TemperAgent — Cedar Authorization Policies
//
// Controls who can create, configure, and interact with spec-driven agents.
// Callback actions are permitted for system agents to enable the dispatch pipeline loop.
// Steering is permitted for supervisors, humans, and parent agents.

// --- Creation and Configuration: admins, supervisors and humans ---

permit(
  principal is Admin,
  action,
  resource is TemperAgent
);

permit(
  principal,
  action in [Action::"create", Action::"Configure", Action::"Provision", Action::"Resume"],
  resource is TemperAgent
) when {
  ["supervisor", "human", "operator"].contains(principal.agent_type)
};

// --- Read and List: any authenticated agent ---

permit(
  principal,
  action in [Action::"read", Action::"list"],
  resource is TemperAgent
);

// --- Steering: supervisors, humans, and system (for parent agents) ---

permit(
  principal,
  action in [Action::"Steer"],
  resource is TemperAgent
) when {
  ["supervisor", "human", "system", "operator"].contains(principal.agent_type)
};

// --- Dispatch pipeline callbacks: system agents ---
// These actions are triggered by WASM integration callbacks, not by users.

permit(
  principal,
  action in [
    Action::"SandboxAllocated",
    Action::"SandboxReady",
    Action::"ProcessToolCalls",
    Action::"HandleToolResults",
    Action::"RecordResult",
    Action::"NeedsCompaction",
    Action::"CompactionComplete",
    Action::"CheckSteering",
    Action::"ContinueWithSteering",
    Action::"FinalizeResult",
    Action::"Heartbeat",
    Action::"TimeoutFail",
    Action::"SandboxLost",
    Action::"SandboxRestored",
    Action::"RestoreFailed",
    Action::"DeletionTeardownSucceeded",
    Action::"DeletionTeardownFailed"
  ],
  resource is TemperAgent
) when {
  principal.agent_type == "system"
};

// --- WASM module HTTP call authorization ---

permit(
  principal is Agent,
  action == Action::"http_call",
  resource is HttpEndpoint
) when {
  ["sandbox_provisioner", "llm_caller", "tool_runner", "workspace_restorer", "sandbox_destroyer", "context_compactor", "steering_checker", "heartbeat_scan", "heartbeat_scheduler", "cron_trigger", "cron_scheduler_check", "cron_scheduler_heartbeat"].contains(context.module)
};

// Allow agent WASM modules to access secrets
permit(
  principal is Agent,
  action == Action::"access_secret",
  resource is Secret
) when {
  ["sandbox_provisioner", "llm_caller", "tool_runner", "workspace_restorer", "sandbox_destroyer", "context_compactor"].contains(context.module)
};

// --- Failure, cancellation, and recovery: supervisors, humans, and system ---

permit(
  principal,
  action in [
    Action::"Fail",
    Action::"Cancel",
    Action::"SandboxLost",
    Action::"RequestDeletion",
    Action::"RetryDeletion"
  ],
  resource is TemperAgent
) when {
  ["supervisor", "human", "system", "operator"].contains(principal.agent_type)
};

// --- Restricted tool policy (Milestone 3) ---
// Deny non-admin, non-supervisor agents from configuring tools that include
// "bash" — only supervisors and humans can grant shell access.
// This is an explicit deny so it overrides any broader permit.

forbid(
  principal,
  action == Action::"Configure",
  resource is TemperAgent
) when {
  !["admin", "supervisor", "human", "operator"].contains(principal.agent_type) &&
  resource.tools_enabled.contains("bash")
};
