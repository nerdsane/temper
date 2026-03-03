import { createTemperTool } from "../src/index.js";
import type { TemperToolConfig } from "../src/types.js";
import * as fs from "node:fs";
import * as path from "node:path";

const TEMPER_URL = process.env.TEMPER_URL ?? "http://localhost:4200";
const TENANT = "default";

/**
 * Demo: Governed Deployment Pipeline
 *
 * This demonstrates a Pi agent that:
 * 1. Loads IOA specs for a deployment pipeline into Temper
 * 2. Creates entities and dispatches actions
 * 3. Effect::Spawn triggers child workflows
 * 4. Each child operates within its Cedar-scoped permissions
 *
 * Prerequisites:
 *   cargo run --bin temper-server -- serve --port 4200 --storage turso --observe
 */
async function main() {
  console.log("=== Temper + Pi: Governed Deployment Pipeline Demo ===\n");

  // Load reference specs from files
  const specsDir = path.join(import.meta.dirname ?? __dirname, "specs");
  const specs: Record<string, string> = {};
  for (const file of fs.readdirSync(specsDir)) {
    if (file.endsWith(".ioa.toml") || file.endsWith(".csdl.xml")) {
      specs[file] = fs.readFileSync(path.join(specsDir, file), "utf-8");
    }
  }

  // Create the lead agent's tool config
  const toolConfig: TemperToolConfig = {
    temperUrl: TEMPER_URL,
    agentHeaders: {
      "X-Temper-Principal-Id": "lead-agent",
      "X-Temper-Principal-Kind": "agent",
      "X-Temper-Agent-Role": "lead_agent",
      "X-Tenant-Id": TENANT,
    },
  };

  const tool = createTemperTool(toolConfig);

  console.log("Step 1: Loading specs into Temper...");

  const specResult = await tool.execute({
    code: `
specs = ${JSON.stringify(specs)}
result = await temper.submit_specs("${TENANT}", specs)
return result
`,
  });
  console.log("  Specs loaded:", JSON.stringify(specResult, null, 2));

  console.log("\nStep 2: Creating LeadPlan entity...");

  const createResult = await tool.execute({
    code: `
entity = await temper.create("${TENANT}", "LeadPlans", {"id": "pipeline-1"})
return entity
`,
  });
  console.log("  Created:", JSON.stringify(createResult, null, 2));

  console.log("\nStep 3: Starting the planning phase...");

  const planResult = await tool.execute({
    code: `
result = await temper.action("${TENANT}", "LeadPlans", "pipeline-1", "StartPlanning", {})
return result
`,
  });
  console.log("  Planning started:", JSON.stringify(planResult, null, 2));

  console.log("\nStep 4: Advancing to testing phase (triggers TestWorkflow spawn)...");

  const testResult = await tool.execute({
    code: `
result = await temper.action("${TENANT}", "LeadPlans", "pipeline-1", "StartTesting", {})
return result
`,
  });
  console.log("  Testing started:", JSON.stringify(testResult, null, 2));

  console.log("\nStep 5: Checking entity state...");

  const stateResult = await tool.execute({
    code: `
plan = await temper.get("${TENANT}", "LeadPlans", "pipeline-1")
return plan
`,
  });
  console.log("  Current state:", JSON.stringify(stateResult, null, 2));

  console.log("\n=== Demo complete ===");
  console.log(
    "The pipeline is now in the Testing phase with a spawned TestWorkflow.",
  );
  console.log(
    "In a real scenario, a child Pi agent would be spawned to execute the TestWorkflow.",
  );
}

main().catch(console.error);
