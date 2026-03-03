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
- await temper.compile_wasm(tenant, module_name, rust_source) — compile Rust source into a WASM module and register it
- await temper.upload_wasm(tenant, module_name, wasm_path) — upload a pre-compiled WASM binary

IOA SPEC FORMAT — FOLLOW THIS EXACTLY:
CRITICAL: Use [automaton] (NOT [entity]). Use initial (NOT status/initial_state). Use [[action]] array tables (NOT [actions.Name]).

[automaton]
name = "EntityName"
states = ["State1", "State2", "State3"]
initial = "State1"

[[action]]
name = "DoSomething"
kind = "input"
from = ["State1"]
to = "State2"
params = ["Param1"]
hint = "Description."

[[action]]
name = "TriggerFetch"
kind = "input"
from = ["State1"]
to = "State2"
effect = "trigger some_integration"

[[action]]
name = "FetchSucceeded"
kind = "input"
from = ["State2"]
to = "State3"
params = ["result_data"]

[[action]]
name = "FetchFailed"
kind = "input"
from = ["State2"]
to = "State1"

[[invariant]]
name = "FinalIsFinal"
when = ["State3"]
assert = "no_further_transitions"

[[integration]]
name = "some_integration"
trigger = "some_integration"
type = "wasm"
module = "http_fetch"
on_success = "FetchSucceeded"
on_failure = "FetchFailed"
url = "https://api.example.com/{param}"
method = "GET"

CSDL FORMAT — REQUIRED alongside IOA specs:
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

SUBMIT EXAMPLE:
await temper.submit_specs(tenant, {"EntityName.ioa.toml": ioa_string, "model.csdl.xml": csdl_string})

CUSTOM WASM MODULES — for capabilities beyond http_fetch:
The built-in http_fetch module handles simple HTTP GET/POST. For anything else (filesystem, custom transforms, multi-step orchestration), write a custom WASM module in Rust using temper-wasm-sdk:

await temper.compile_wasm(tenant, "my_module", rust_source)

The Rust source uses temper_wasm_sdk::prelude::* and the temper_module! macro:

use temper_wasm_sdk::prelude::*;
temper_module! {
    fn run(ctx: Context) -> Result<Value> {
        // ctx.config — BTreeMap<String, String> from [[integration]] config keys
        // ctx.trigger_params — JSON Value from the triggering action's params
        // ctx.entity_state — current entity state snapshot
        // ctx.http_get(url) -> Result<HttpResponse>
        // ctx.http_post(url, body) -> Result<HttpResponse>
        // ctx.http_call(method, url, headers, body) -> Result<HttpResponse>
        // ctx.get_secret(key) -> Result<String>
        // ctx.log(level, msg)
        // HttpResponse has .status (u16) and .body (String)
        let resp = ctx.http_get(&ctx.config["url"])?;
        let data: Value = serde_json::from_str(&resp.body)?;
        Ok(json!({ "result": data }))
    }
}

After compile_wasm succeeds, reference the module in [[integration]] sections:
[[integration]]
name = "my_custom_op"
trigger = "my_custom_op"
type = "wasm"
module = "my_module"
on_success = "OpSucceeded"
on_failure = "OpFailed"
custom_config_key = "some_value"

On success the returned Value becomes the callback action's params. On error the on_failure action fires with {"error": "..."}.
Compiler errors are returned for self-correction — fix and resubmit.

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

For HTTP APIs, use type = "wasm" with module = "http_fetch" (built-in).
For anything else (filesystem, custom transforms, multi-step logic), write a custom WASM module:

\`\`\`python
rust_src = '''
use temper_wasm_sdk::prelude::*;
temper_module! {
    fn run(ctx: Context) -> Result<Value> {
        // ctx.config — config keys from [[integration]]
        // ctx.trigger_params — params from triggering action
        // ctx.http_get(url) / ctx.http_post(url, body) / ctx.http_call(method, url, headers, body)
        // ctx.get_secret(key) — read secrets
        // ctx.log(level, msg) — logging
        // Return Ok(json_value) for on_success callback, Err(string) for on_failure
        let resp = ctx.http_get(&ctx.config["url"])?;
        Ok(serde_json::from_str(&resp.body)?)
    }
}
'''
result = await temper.compile_wasm("${TENANT}", "my_module", rust_src)
# Returns {"status":"compiled","module":"my_module","hash":"...","size":...} on success
# Returns compiler errors on failure — fix and retry
\`\`\`

Then reference it in specs: module = "my_module" in [[integration]] sections.

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
