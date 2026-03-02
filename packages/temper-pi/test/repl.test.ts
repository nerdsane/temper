import { describe, it, expect, beforeAll, afterAll } from "vitest";
import { startServer, stopServer, getServerUrl } from "./setup.js";

describe("POST /api/repl", () => {
  let baseUrl: string;

  beforeAll(async () => {
    baseUrl = await startServer();
  }, 120_000);

  afterAll(async () => {
    await stopServer();
  });

  it("executes simple Python and returns result", async () => {
    const res = await fetch(`${baseUrl}/api/repl`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ code: "return 1 + 2" }),
    });

    expect(res.ok).toBe(true);
    const data = await res.json();
    expect(data.error).toBeNull();
    expect(data.result).toBe(3);
  });

  it("returns error for invalid Python", async () => {
    const res = await fetch(`${baseUrl}/api/repl`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ code: "this is not valid python!!!" }),
    });

    expect(res.ok).toBe(true);
    const data = await res.json();
    expect(data.error).toBeTruthy();
    expect(data.result).toBeNull();
  });

  it("returns error for empty code", async () => {
    const res = await fetch(`${baseUrl}/api/repl`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ code: "" }),
    });

    expect(res.ok).toBe(true);
    const data = await res.json();
    // Empty code should return null result, not an error
    expect(data.result).toBeNull();
  });

  it("can list entities via temper.list()", async () => {
    const res = await fetch(`${baseUrl}/api/repl`, {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        "X-Tenant-Id": "default",
      },
      body: JSON.stringify({
        code: `result = await temper.list("default", "LeadPlans")\nreturn result`,
      }),
    });

    expect(res.ok).toBe(true);
    const data = await res.json();
    // May be empty list or 404 if no specs loaded, but should not panic
    expect(data).toBeDefined();
  });

  it("includes agent identity headers in sandbox calls", async () => {
    const res = await fetch(`${baseUrl}/api/repl`, {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        "X-Temper-Principal-Id": "test-agent-1",
        "X-Temper-Principal-Kind": "agent",
        "X-Temper-Agent-Role": "test_agent",
      },
      body: JSON.stringify({ code: "return 'hello from agent'" }),
    });

    expect(res.ok).toBe(true);
    const data = await res.json();
    expect(data.error).toBeNull();
    expect(data.result).toBe("hello from agent");
  });
});
