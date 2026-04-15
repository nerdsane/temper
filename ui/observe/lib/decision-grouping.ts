import type { PendingDecision } from "./types";

export type GroupingStrategy = "action_resource" | "agent_action" | "agent_type_action";

function groupKey(decision: PendingDecision, strategy: GroupingStrategy): string {
  switch (strategy) {
    case "action_resource":
      return `${decision.action}::${decision.resource_type}`;
    case "agent_action":
      return `${decision.agent_id}::${decision.action}`;
    case "agent_type_action":
      return `${decision.agent_type || "unknown"}::${decision.action}`;
  }
}

export function groupLabel(key: string, strategy: GroupingStrategy): string {
  const [first, second] = key.split("::");
  switch (strategy) {
    case "action_resource":
      return `${first} on ${second}`;
    case "agent_action":
      return `${first} / ${second}`;
    case "agent_type_action":
      return `${first} / ${second}`;
  }
}

export function groupDecisions(
  decisions: PendingDecision[],
  strategy: GroupingStrategy,
): Map<string, PendingDecision[]> {
  const groups = new Map<string, PendingDecision[]>();
  for (const d of decisions) {
    const key = groupKey(d, strategy);
    const list = groups.get(key);
    if (list) {
      list.push(d);
    } else {
      groups.set(key, [d]);
    }
  }
  return groups;
}

/** Extract common context from a group of decisions for use in PolicyBuilder standalone mode. */
export function commonContext(decisions: PendingDecision[]): {
  agentId?: string;
  agentType?: string;
  action?: string;
  resourceType?: string;
  sessionId?: string;
} {
  if (decisions.length === 0) return {};

  const first = decisions[0];
  const allSameAgent = decisions.every((d) => d.agent_id === first.agent_id);
  const allSameAgentType = decisions.every((d) => d.agent_type === first.agent_type);
  const allSameAction = decisions.every((d) => d.action === first.action);
  const allSameResourceType = decisions.every((d) => d.resource_type === first.resource_type);
  const allSameSession = decisions.every((d) => d.session_id === first.session_id);

  return {
    agentId: allSameAgent ? first.agent_id : undefined,
    agentType: allSameAgentType ? first.agent_type : undefined,
    action: allSameAction ? first.action : undefined,
    resourceType: allSameResourceType ? first.resource_type : undefined,
    sessionId: allSameSession ? first.session_id : undefined,
  };
}
