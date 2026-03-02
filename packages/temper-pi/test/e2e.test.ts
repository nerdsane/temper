import { describe, it, expect, beforeAll, afterAll } from "vitest";
import { startServer, stopServer, getServerUrl } from "./setup.js";
import { createTemperAgent } from "../src/agent.js";

describe("E2E: Agent session", () => {
  let baseUrl: string;

  beforeAll(async () => {
    baseUrl = await startServer();
  }, 120_000);

  afterAll(async () => {
    await stopServer();
  });

  it("creates a valid agent session config", async () => {
    const session = await createTemperAgent({
      temperUrl: baseUrl,
      tenant: "default",
      principalId: "e2e-agent",
      role: "test_agent",
      model: "claude-sonnet-4-20250514",
      task: "Test task for E2E validation",
    });

    expect(session.tools).toHaveLength(1);
    expect(session.tools[0].name).toBe("temper");
    expect(session.model).toBe("claude-sonnet-4-20250514");
    expect(session.provider).toBe("anthropic");
    expect(session.systemPrompt).toContain("e2e-agent");
    expect(session.systemPrompt).toContain("test_agent");
  });

  it("session tool can execute REPL code", async () => {
    const session = await createTemperAgent({
      temperUrl: baseUrl,
      tenant: "default",
      principalId: "e2e-agent",
      role: "test_agent",
      model: "claude-sonnet-4-20250514",
      task: "Execute a simple test",
    });

    const tool = session.tools[0];
    const result = await tool.execute({ code: "return 42" });
    expect(result).toBeDefined();
  });

  it("infers provider from model name", async () => {
    const anthropicSession = await createTemperAgent({
      temperUrl: baseUrl,
      tenant: "default",
      principalId: "test",
      role: "test",
      model: "claude-sonnet-4-20250514",
      task: "test",
    });
    expect(anthropicSession.provider).toBe("anthropic");

    const openaiSession = await createTemperAgent({
      temperUrl: baseUrl,
      tenant: "default",
      principalId: "test",
      role: "test",
      model: "gpt-4o",
      task: "test",
    });
    expect(openaiSession.provider).toBe("openai");

    const googleSession = await createTemperAgent({
      temperUrl: baseUrl,
      tenant: "default",
      principalId: "test",
      role: "test",
      model: "gemini-2.0-flash",
      task: "test",
    });
    expect(googleSession.provider).toBe("google");
  });
});
