// Lead agent can manage the deployment pipeline
permit(
  principal == Agent::"lead-agent",
  action in [Action::"dispatch_action", Action::"create_entity"],
  resource in EntityType::"LeadPlan"
);

// Lead agent can load specs
permit(
  principal == Agent::"lead-agent",
  action == Action::"load_specs",
  resource
);

// Test agent: scoped to TestWorkflow entities only
permit(
  principal == Agent::"test-agent",
  action in [Action::"dispatch_action"],
  resource in EntityType::"TestWorkflow"
);

// Review agent: scoped to ReviewWorkflow entities only
permit(
  principal == Agent::"review-agent",
  action in [Action::"dispatch_action"],
  resource in EntityType::"ReviewWorkflow"
);

// Deploy agent: scoped to DeployWorkflow entities only
permit(
  principal == Agent::"deploy-agent",
  action in [Action::"dispatch_action"],
  resource in EntityType::"DeployWorkflow"
);

// Deny all agents from policy management (human-only)
forbid(
  principal,
  action == Action::"manage_policies",
  resource
) when { principal is Agent };

// Deny all agents from approving/denying decisions (human-only)
forbid(
  principal,
  action in [Action::"approve_decision", Action::"deny_decision"],
  resource
) when { principal is Agent };

// Lead agent: full access to Pipeline, WorkItem, AgentSession
permit(
  principal == Agent::"lead-agent",
  action in [Action::"dispatch_action", Action::"create_entity"],
  resource in EntityType::"Pipeline"
);

permit(
  principal == Agent::"lead-agent",
  action in [Action::"dispatch_action", Action::"create_entity"],
  resource in EntityType::"WorkItem"
);

permit(
  principal == Agent::"lead-agent",
  action in [Action::"dispatch_action", Action::"create_entity"],
  resource in EntityType::"AgentSession"
);

// Worker agent: limited operations on WorkItem and AgentSession
permit(
  principal == Agent::"worker-agent",
  action == Action::"dispatch_action",
  resource in EntityType::"WorkItem"
);

permit(
  principal == Agent::"worker-agent",
  action == Action::"dispatch_action",
  resource in EntityType::"AgentSession"
);
