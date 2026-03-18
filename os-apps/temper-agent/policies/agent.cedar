// TemperAgent — Cedar Authorization Policies
//
// Controls who can create, configure, and interact with spec-driven agents.
// Callback actions (SandboxReady, ProcessToolCalls, HandleToolResults, RecordResult)
// are permitted for system agents to enable the dispatch pipeline loop.

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
  ["supervisor", "human"].contains(principal.agent_type)
};

// --- Read and List: any authenticated agent ---

permit(
  principal,
  action in [Action::"read", Action::"list"],
  resource is TemperAgent
);

// --- Dispatch pipeline callbacks: system agents ---
// These actions are triggered by WASM integration callbacks, not by users.
// The system agent identity drives the callback dispatch.

permit(
  principal,
  action in [
    Action::"SandboxReady",
    Action::"ProcessToolCalls",
    Action::"HandleToolResults",
    Action::"RecordResult"
  ],
  resource is TemperAgent
) when {
  principal.agent_type == "system"
};

// --- WASM module HTTP call authorization ---
// Allow agent WASM modules to call the Anthropic API
permit(
  principal is Agent,
  action == Action::"http_call",
  resource is HttpEndpoint
) when {
  ["sandbox_provisioner", "llm_caller", "tool_runner", "workspace_restorer"].contains(context.module)
};

// Allow agent WASM modules to access secrets
permit(
  principal is Agent,
  action == Action::"access_secret",
  resource is Secret
) when {
  ["sandbox_provisioner", "llm_caller", "tool_runner", "workspace_restorer"].contains(context.module)
};

// --- Failure and cancellation: supervisors, humans, and system ---

permit(
  principal,
  action in [Action::"Fail", Action::"Cancel"],
  resource is TemperAgent
) when {
  ["supervisor", "human", "system"].contains(principal.agent_type)
};
