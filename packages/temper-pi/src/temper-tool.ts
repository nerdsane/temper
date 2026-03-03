import type { AgentTool, TemperToolConfig, ReplResponse } from "./types.js";

/**
 * Create the Temper REPL tool for a Pi agent-core session.
 *
 * This is the agent's ONLY tool. It POSTs Python code to the Temper server's
 * `/api/repl` endpoint, which runs it in the Monty sandbox with access to
 * `temper.*` methods (create, action, submit_specs, etc.) and `spec.*` methods
 * (tenants, entities, describe, actions, etc.).
 *
 * Security: The Monty sandbox enforces 180s timeout, 64MB memory limit,
 * method allowlisting, and no filesystem/network access. External APIs
 * go through [[integration]] sections in IOA specs.
 */
export function createTemperTool(config: TemperToolConfig): AgentTool {
  return {
    name: "temper",
    description: `Python REPL connected to the Temper governance server. This is your ONLY tool.

A \`temper\` object is available with these methods:
- temper.submit_specs(tenant, specs) — load IOA + CSDL specs (triggers verification)
- temper.create(tenant, entity_set, fields) — create a new entity
- temper.action(tenant, entity_set, id, action, params) — dispatch an action (Cedar-gated)
- temper.get(tenant, entity_set, id) — read entity state
- temper.list(tenant, entity_set) — list entities
- temper.poll_decision(tenant, decision_id) — wait for human approval
- temper.get_decisions(tenant) — list pending decisions
- temper.get_trajectories(tenant) — get evolution trajectory data
- temper.get_insights(tenant) — get ranked evolution insights
- temper.get_policies(tenant) — get current Cedar policies
- temper.show_spec(tenant, entity_type) — show entity spec
- temper.patch(tenant, entity_set, id, fields) — update entity fields

Write Python code. Use \`await\` for all temper methods. Return results with \`return\`.

If an action is denied by Cedar policy:
  result = await temper.action(...)
  if isinstance(result, dict) and result.get("status") == "authorization_denied":
      decision = await temper.poll_decision(tenant, result["decision_id"])
      # Human approves → retry the action`,
    parameters: {
      type: "object",
      properties: {
        code: {
          type: "string",
          description: "Python code to execute in the Temper sandbox",
        },
      },
      required: ["code"],
    },
    async execute(params: Record<string, unknown>): Promise<unknown> {
      const code = params.code as string;
      if (!code || typeof code !== "string") {
        return { result: null, error: "Missing or invalid 'code' parameter" };
      }

      try {
        const response = await fetch(`${config.temperUrl}/api/repl`, {
          method: "POST",
          headers: {
            "Content-Type": "application/json",
            ...config.agentHeaders,
          },
          body: JSON.stringify({ code }),
        });

        const data: ReplResponse = await response.json();
        return data;
      } catch (err) {
        return {
          result: null,
          error: `REPL request failed: ${err instanceof Error ? err.message : String(err)}`,
        };
      }
    },
  };
}
