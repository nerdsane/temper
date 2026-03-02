import { describe, it, expect, beforeAll, afterAll } from "vitest";
import { startServer, stopServer, getServerUrl } from "./setup.js";
import { createTemperTool } from "../src/temper-tool.js";
import { createTemperToolForAgent } from "../src/agent.js";

describe("E2E: Temper REPL tool", () => {
  let baseUrl: string;

  beforeAll(async () => {
    baseUrl = await startServer();
  }, 120_000);

  afterAll(async () => {
    await stopServer();
  });

  it("createTemperTool returns callable execute", async () => {
    const tool = createTemperTool({
      temperUrl: baseUrl,
      agentHeaders: {
        "X-Temper-Principal-Id": "e2e-agent",
        "X-Temper-Principal-Kind": "agent",
        "X-Temper-Agent-Role": "test_agent",
        "X-Tenant-Id": "default",
      },
    });

    const result = await tool.execute({ code: "return 42" });
    expect(result).toBeDefined();
    expect(result.error).toBeNull();
  });

  it("createTemperToolForAgent creates tool from agent config", async () => {
    const tool = createTemperToolForAgent({
      temperUrl: baseUrl,
      tenant: "default",
      principalId: "e2e-agent",
      role: "test_agent",
      model: "claude-sonnet-4-20250514",
      task: "test",
    });

    const result = await tool.execute({ code: "return 'hello'" });
    expect(result).toBeDefined();
    expect(result.error).toBeNull();
  });

  it("REPL returns error for invalid code", async () => {
    const tool = createTemperTool({
      temperUrl: baseUrl,
      agentHeaders: {
        "X-Temper-Principal-Id": "e2e-agent",
        "X-Temper-Principal-Kind": "agent",
        "X-Temper-Agent-Role": "test_agent",
        "X-Tenant-Id": "default",
      },
    });

    const result = await tool.execute({ code: "raise Exception('test error')" });
    expect(result).toBeDefined();
    expect(result.error).not.toBeNull();
  });
});
