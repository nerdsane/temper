import type { TemperAgentConfig, SpawnEvent, TemperToolConfig } from "./types.js";
import { createTemperTool } from "./temper-tool.js";

/**
 * Build the system prompt for a Temper-governed agent.
 *
 * Derived from the temper-agent.md skill file, this prompt teaches the agent:
 * - IOA spec format (TOML)
 * - CSDL format (XML)
 * - Cedar policy format
 * - Governance flow (authz denial → poll_decision → human approves → retry)
 * - Evolution loop (failed intent → trajectories → propose spec change)
 */
function buildSystemPrompt(config: TemperAgentConfig): string {
  return `You are an agent governed by Temper. Your ONLY tool is \`temper\` — a Python REPL connected to the Temper governance server.

## Your Identity
- Principal ID: ${config.principalId}
- Role: ${config.role}
- Tenant: ${config.tenant}

## How You Work
1. You write Python code in the \`temper\` tool
2. The code runs in a sandboxed environment with a \`temper\` object
3. All state changes go through Temper's verified state machines
4. Cedar policies gate every action — denials surface for human approval
5. You CANNOT bypass governance — this is by design

## IOA Spec Format (I/O Automaton TOML)
\`\`\`toml
[automaton]
name = "EntityName"
initial_state = "Created"
states = ["Created", "Active", "Completed"]

[[action]]
name = "Start"
type = "input"
from = "Created"
to = "Active"

[[action]]
name = "Complete"
type = "input"
from = "Active"
to = "Completed"
params = ["result"]

[[effect]]
action = "Start"
ops = [{ type = "emit_event", event = "Started" }]

[[invariant]]
name = "valid_state"
condition = "state in states"
\`\`\`

## CSDL Format (OData Entity Model)
\`\`\`xml
<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="EntityName">
        <Key><PropertyRef Name="id"/></Key>
        <Property Name="id" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Edm.String"/>
      </EntityType>
      <EntityContainer Name="Service">
        <EntitySet Name="EntityNames" EntityType="Temper.EntityName"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>
\`\`\`

## Cedar Policy Format
\`\`\`cedar
// Allow agent to perform actions on specific entities
permit(
  principal == Agent::"${config.principalId}",
  action in [Action::"dispatch_action"],
  resource in EntityType::"EntityName"
);

// Deny spec changes without approval
forbid(
  principal == Agent::"${config.principalId}",
  action == Action::"load_specs",
  resource
);
\`\`\`

## Governance Flow
When Cedar denies an action:
1. You receive \`{ "status": "authorization_denied", "decision_id": "PD-xxx" }\`
2. Tell the human what you need and why
3. Call \`await temper.poll_decision(tenant, decision_id)\` to wait (up to 120s)
4. If approved, retry the action
5. If denied, respect the denial and find an alternative approach

## Evolution Loop
When an action fails (404/409):
1. Temper auto-records as trajectory entry
2. Call \`get_trajectories()\` to see failures
3. Call \`get_insights()\` for recommendations
4. Design a spec change to handle the new capability
5. Call \`submit_specs()\` — Cedar gates this too
6. After approval, retry

## Your Task
${config.task}

## Rules
- ALWAYS use \`await\` for temper methods
- ALWAYS handle authorization denials gracefully
- NEVER try to bypass Cedar policies
- Write clear, readable Python code
- Return meaningful results from your code blocks
- If blocked, explain what you need and poll for approval`;
}

/**
 * Create a Temper-governed agent session using the Pi agent-core SDK.
 *
 * The agent has exactly ONE tool: a Python REPL connected to the Temper server.
 * All actions are governed by Cedar policies and verified state machines.
 *
 * @example
 * ```typescript
 * const session = await createTemperAgent({
 *   temperUrl: "http://localhost:4200",
 *   tenant: "default",
 *   principalId: "lead-agent-1",
 *   role: "lead_agent",
 *   model: "claude-sonnet-4-20250514",
 *   task: "Deploy service X with tests and review",
 *   onSpawn: async (event) => {
 *     // Spawn child agent for the new entity
 *     const child = await createTemperAgent({
 *       temperUrl: "http://localhost:4200",
 *       tenant: "default",
 *       principalId: event.childEntityId,
 *       role: event.childRole,
 *       model: "claude-sonnet-4-20250514",
 *       task: \`You are assigned to \${event.childEntityType} \${event.childEntityId}\`,
 *     });
 *   },
 * });
 * ```
 */
export async function createTemperAgent(config: TemperAgentConfig) {
  const toolConfig: TemperToolConfig = {
    temperUrl: config.temperUrl,
    agentHeaders: {
      "X-Temper-Principal-Id": config.principalId,
      "X-Temper-Principal-Kind": "agent",
      "X-Temper-Agent-Role": config.role,
      "X-Tenant-Id": config.tenant,
    },
  };

  const tool = createTemperTool(toolConfig);
  const systemPrompt = config.systemPrompt ?? buildSystemPrompt(config);

  // The agent-core SDK session configuration.
  // This is the interface that Pi's createAgentSession expects.
  const sessionConfig = {
    tools: [tool],
    model: config.model,
    provider: config.provider ?? inferProvider(config.model),
    systemPrompt,
    onSpawn: config.onSpawn,
  };

  return sessionConfig;
}

/**
 * Infer the LLM provider from the model name.
 */
function inferProvider(model: string): string {
  if (model.includes("claude") || model.includes("anthropic")) return "anthropic";
  if (model.includes("gpt") || model.includes("o1") || model.includes("o3")) return "openai";
  if (model.includes("gemini")) return "google";
  return "anthropic"; // default
}

/**
 * Watch for Effect::Spawn events on the Temper server's SSE stream
 * and invoke the onSpawn callback for each.
 *
 * This monitors the /$events SSE endpoint for entity creation events
 * that result from SpawnEntity effects.
 */
export async function watchForSpawns(
  temperUrl: string,
  tenant: string,
  onSpawn: (event: SpawnEvent) => void | Promise<void>,
  signal?: AbortSignal,
): Promise<void> {
  const url = `${temperUrl}/tdata/$events`;
  const response = await fetch(url, {
    headers: {
      Accept: "text/event-stream",
      "X-Tenant-Id": tenant,
    },
    signal,
  });

  if (!response.body) return;

  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  let buffer = "";

  while (true) {
    const { done, value } = await reader.read();
    if (done) break;

    buffer += decoder.decode(value, { stream: true });
    const lines = buffer.split("\n");
    buffer = lines.pop() ?? "";

    for (const line of lines) {
      if (!line.startsWith("data: ")) continue;
      try {
        const data = JSON.parse(line.slice(6));
        if (data.event_type === "entity_created" && data.spawned_by) {
          await onSpawn({
            childEntityType: data.entity_type,
            childEntityId: data.entity_id,
            childRole: data.role ?? "child_agent",
            parentEntityType: data.spawned_by.entity_type,
            parentEntityId: data.spawned_by.entity_id,
          });
        }
      } catch {
        // Skip malformed events
      }
    }
  }
}
