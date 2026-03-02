/**
 * Pi extension that registers the Temper REPL as the sole agent tool.
 *
 * Usage:
 *   pi --no-tools --no-extensions -e ./src/extension.ts
 *
 * Environment variables:
 *   TEMPER_URL       — Temper server URL (default: http://localhost:4200)
 *   TEMPER_TENANT    — Tenant ID (default: default)
 *   TEMPER_PRINCIPAL — Agent principal ID (default: lead-agent)
 *   TEMPER_ROLE      — Agent role (default: lead_agent)
 */
import { Type } from "@mariozechner/pi-ai";
import type { ExtensionAPI } from "@mariozechner/pi-coding-agent";
import type { ReplResponse } from "./types.js";

const TEMPER_URL = process.env.TEMPER_URL ?? "http://localhost:4200";
const TENANT = process.env.TEMPER_TENANT ?? "default";
const PRINCIPAL = process.env.TEMPER_PRINCIPAL ?? "lead-agent";
const ROLE = process.env.TEMPER_ROLE ?? "lead_agent";

const headers: Record<string, string> = {
  "Content-Type": "application/json",
  "X-Temper-Principal-Id": PRINCIPAL,
  "X-Temper-Principal-Kind": "agent",
  "X-Temper-Agent-Role": ROLE,
  "X-Tenant-Id": TENANT,
};

export default function (pi: ExtensionAPI) {
  pi.registerTool({
    name: "temper",
    label: "Temper",
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

A \`spec\` object is available for read-only queries:
- spec.tenants() — list tenants
- spec.entities(tenant) — list entity types
- spec.describe(tenant, entity_type) — full entity description
- spec.actions(tenant, entity_type) — list actions
- spec.actions_from(tenant, entity_type, state) — actions available from a state

Write Python code. Use \`await\` for all methods. Return results with \`return\`.

If an action is denied by Cedar policy, you get { "status": "authorization_denied", "decision_id": "PD-xxx" }.
Call temper.poll_decision(tenant, decision_id) to wait for human approval, then retry.`,
    parameters: Type.Object({
      code: Type.String({
        description: "Python code to execute in the Temper sandbox",
      }),
    }),
    async execute(_toolCallId, params, _signal, _onUpdate) {
      try {
        const response = await fetch(`${TEMPER_URL}/api/repl`, {
          method: "POST",
          headers,
          body: JSON.stringify({ code: params.code }),
        });
        const data: ReplResponse = await response.json();
        if (data.error) {
          return {
            content: [{ type: "text" as const, text: `Error: ${data.error}` }],
          };
        }
        return {
          content: [
            {
              type: "text" as const,
              text:
                typeof data.result === "string"
                  ? data.result
                  : JSON.stringify(data.result, null, 2),
            },
          ],
        };
      } catch (err) {
        return {
          content: [
            {
              type: "text" as const,
              text: `REPL request failed: ${err instanceof Error ? err.message : String(err)}`,
            },
          ],
        };
      }
    },
  });

  // Inject Temper governance context into the system prompt via before_agent_start event
  pi.on("before_agent_start", async (event) => {
    event.systemPrompt = `You are an agent governed by Temper. Your ONLY tool is \`temper\` — a Python REPL connected to the Temper governance server at ${TEMPER_URL}.

Your identity: Principal "${PRINCIPAL}", Role "${ROLE}", Tenant "${TENANT}".

All state changes go through Temper's verified state machines. Cedar policies gate every action — denials surface for human approval. You CANNOT bypass governance.

When Cedar denies an action, you receive { "status": "authorization_denied", "decision_id": "PD-xxx" }. Tell the human what you need, then call temper.poll_decision("${TENANT}", decision_id) to wait for approval.

When an action fails (404/409), check temper.get_trajectories("${TENANT}") and temper.get_insights("${TENANT}") for recommendations, then propose a spec change via temper.submit_specs().`;
  });
}
