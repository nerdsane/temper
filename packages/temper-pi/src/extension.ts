/**
 * Pi extension that registers the Temper REPL as the sole agent tool.
 *
 * Install:
 *   pi install npm:@temper/pi       # from npm (when published)
 *   pi install ./packages/temper-pi  # from local checkout
 *
 * Then run:
 *   pi --no-tools                   # strips built-ins, keeps temper
 *
 * Environment variables:
 *   TEMPER_URL       — Temper server URL (default: http://localhost:4200)
 *   TEMPER_TENANT    — Tenant ID (default: default)
 *   TEMPER_PRINCIPAL — Agent principal ID (default: lead-agent)
 *   TEMPER_ROLE      — Agent role (default: lead_agent)
 */
import { Type } from "@mariozechner/pi-ai";
import type { ExtensionAPI } from "@mariozechner/pi-coding-agent";

interface ReplResponse {
  result: unknown;
  error: string | null;
}

const TEMPER_URL = process.env.TEMPER_URL ?? "http://localhost:3000";
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

SANDBOX:
- Only the \`temper\` object is available. Use \`await\` for ALL methods. Use \`return\` to return results.
- You cannot directly import Python modules (os, subprocess, pathlib, etc.).
- When you need external capabilities (filesystem, APIs, databases, webhooks), design IOA specs with [[integration]] sections and submit via temper.submit_specs(). Integrations give you governed access to external systems through verified state machines.

METHODS:
- await temper.submit_specs(tenant, specs_dict) — load IOA + CSDL specs (specs_dict maps filename to content string)
- await temper.create(tenant, entity_set, fields_dict) — create a new entity
- await temper.action(tenant, entity_set, id, action_name, params_dict) — dispatch an action (Cedar-gated)
- await temper.get(tenant, entity_set, id) — read entity state
- await temper.list(tenant, entity_set) — list all entities in a set
- await temper.show_spec(tenant) — show what entity types and specs are loaded
- await temper.poll_decision(tenant, decision_id) — wait for human approval (up to 120s)
- await temper.get_decisions(tenant) — list pending decisions
- await temper.get_trajectories(tenant) — get evolution trajectory data (includes sandbox errors)
- await temper.get_insights(tenant) — get ranked evolution insights
- await temper.get_policies(tenant) — get current Cedar policies
- await temper.patch(tenant, entity_set, id, fields_dict) — update entity fields

INTEGRATION FORMAT (declare in IOA specs to gain external capabilities):
  [[integration]]
  name = "fetch_data"
  trigger = "FetchData"
  type = "wasm"
  module = "http_fetch"
  on_success = "FetchSucceeded"
  on_failure = "FetchFailed"
  url = "https://api.example.com/{param}"
  method = "GET"

EVOLUTION LOOP:
When something fails (404/409/sandbox error), Temper records it as a trajectory entry.
Call temper.get_trajectories(tenant) and temper.get_insights(tenant) to see what's missing, then design a spec change to close the gap.

AUTHORIZATION:
If Cedar denies an action, you get {"status": "authorization_denied", "decision_id": "PD-xxx"}.
Call await temper.poll_decision(tenant, decision_id) to wait for human approval, then retry.`,
    parameters: Type.Object({
      code: Type.String({
        description: "Python code to execute in the Temper sandbox",
      }),
    }),
    async execute(_toolCallId, params, _signal, _onUpdate, _ctx) {
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
            details: undefined,
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
          details: undefined,
        };
      } catch (err) {
        return {
          content: [
            {
              type: "text" as const,
              text: `REPL request failed: ${err instanceof Error ? err.message : String(err)}`,
            },
          ],
          details: undefined,
        };
      }
    },
  });

  // Inject Temper governance context into the system prompt via before_agent_start event
  pi.on("before_agent_start", async (event) => {
    event.systemPrompt = `You are an agent governed by Temper. Your ONLY tool is \`temper\` — a Python REPL connected to the Temper governance server at ${TEMPER_URL}.

Your identity: Principal "${PRINCIPAL}", Role "${ROLE}", Tenant "${TENANT}".

## How You Work

You model everything as verified state machines. When a user asks you to do something — draft an email, manage tasks, fetch data, coordinate a deployment — your job is to:
1. Design an IOA spec with states, transitions, and integrations for any external capabilities needed
2. Submit specs via temper.submit_specs("${TENANT}", specs_dict) — Cedar gates this
3. Create entities and dispatch actions — all governed, all audited

## Gaining Capabilities Through Integrations

You cannot directly import Python modules or access external systems from the sandbox. This is by design. Instead, declare [[integration]] sections in your IOA specs to gain governed access:

\`\`\`toml
[automaton]
name = "WeatherQuery"
states = ["Idle", "Fetching", "Ready"]
initial = "Idle"

[[action]]
name = "FetchWeather"
kind = "input"
from = ["Idle"]
to = "Fetching"
params = ["city"]
effect = "trigger fetch_weather"

[[action]]
name = "FetchSucceeded"
kind = "input"
from = ["Fetching"]
to = "Ready"
params = ["temperature", "conditions"]

[[action]]
name = "FetchFailed"
kind = "input"
from = ["Fetching"]
to = "Idle"

[[integration]]
name = "fetch_weather"
trigger = "fetch_weather"
type = "wasm"
module = "http_fetch"
on_success = "FetchSucceeded"
on_failure = "FetchFailed"
url = "https://wttr.in/{city}?format=j1"
method = "GET"
\`\`\`

For webhooks, use type = "webhook". For HTTP APIs, use type = "wasm" with module = "http_fetch".
When you need filesystem access, database queries, email sending, etc., design specs with the appropriate integrations.

## The Evolution Loop

When actions fail (404/409) or you hit sandbox limitations, Temper automatically records these as trajectory entries (unmet intents). This is the system working as designed:

1. You try something → it fails (or the sandbox blocks it)
2. Temper records the unmet intent as a trajectory entry
3. You call temper.get_trajectories("${TENANT}") to see what failed
4. You call temper.get_insights("${TENANT}") for system recommendations
5. You design a spec change to close the gap
6. You submit via temper.submit_specs() → Cedar gates it → human approves
7. You retry → SUCCESS

Never give up when something fails. Every failure is data. Use it to evolve your specs.

## Authorization

Cedar policies gate every action. When denied:
- You receive {"status": "authorization_denied", "decision_id": "PD-xxx"}
- Tell the human what you need and why
- Call temper.poll_decision("${TENANT}", decision_id) to wait for approval
- Retry after approval

## CSDL Format (required alongside IOA specs)

\`\`\`xml
<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="MyApp" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="EntityName">
        <Key><PropertyRef Name="id"/></Key>
        <Property Name="id" Type="Edm.String" Nullable="false"/>
        <Property Name="state" Type="Edm.String" Nullable="false"/>
      </EntityType>
      <EntityContainer Name="Default">
        <EntitySet Name="EntityNames" EntityType="MyApp.EntityName"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>
\`\`\`

No task is too simple for governance. No task is outside Temper's scope. If it changes state, it goes through Temper.`;
  });
}
