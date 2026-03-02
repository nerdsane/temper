import { describe, it, expect, beforeAll, afterAll } from "vitest";
import { startServer, stopServer, getServerUrl } from "./setup.js";
import { createTemperTool } from "../src/temper-tool.js";
import type { TemperToolConfig } from "../src/types.js";

describe("Governance integration", () => {
  let baseUrl: string;
  let leadTool: ReturnType<typeof createTemperTool>;
  let testAgentTool: ReturnType<typeof createTemperTool>;

  beforeAll(async () => {
    baseUrl = await startServer();

    // Lead agent tool
    leadTool = createTemperTool({
      temperUrl: baseUrl,
      agentHeaders: {
        "X-Temper-Principal-Id": "lead-agent",
        "X-Temper-Principal-Kind": "agent",
        "X-Temper-Agent-Role": "lead_agent",
        "X-Tenant-Id": "default",
      },
    });

    // Test agent tool (restricted)
    testAgentTool = createTemperTool({
      temperUrl: baseUrl,
      agentHeaders: {
        "X-Temper-Principal-Id": "test-agent",
        "X-Temper-Principal-Kind": "agent",
        "X-Temper-Agent-Role": "test_agent",
        "X-Tenant-Id": "default",
      },
    });
  }, 120_000);

  afterAll(async () => {
    await stopServer();
  });

  it("can submit specs via REPL tool", async () => {
    const result = await leadTool.execute({
      code: `
specs = {
    "SimpleEntity.ioa.toml": """
[automaton]
name = "SimpleEntity"
initial_state = "Created"
states = ["Created", "Active"]

[[action]]
name = "Activate"
type = "input"
from = "Created"
to = "Active"
""",
    "model.csdl.xml": """<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="SimpleEntity">
        <Key><PropertyRef Name="id"/></Key>
        <Property Name="id" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Edm.String"/>
      </EntityType>
      <EntityContainer Name="Service">
        <EntitySet Name="SimpleEntities" EntityType="Temper.SimpleEntity"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"""
}
result = await temper.submit_specs("default", specs)
return result
`,
    });

    expect(result).toBeDefined();
  });

  it("can create and read entities", async () => {
    // Create
    const createResult = await leadTool.execute({
      code: `
entity = await temper.create("default", "SimpleEntities", {"id": "test-1"})
return entity
`,
    });
    expect(createResult).toBeDefined();

    // Read
    const getResult = await leadTool.execute({
      code: `
entity = await temper.get("default", "SimpleEntities", "test-1")
return entity
`,
    });
    expect(getResult).toBeDefined();
  });

  it("can dispatch actions", async () => {
    const result = await leadTool.execute({
      code: `
result = await temper.action("default", "SimpleEntities", "test-1", "Activate", {})
return result
`,
    });
    expect(result).toBeDefined();
  });
});
