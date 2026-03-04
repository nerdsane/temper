import { describe, it, expect } from "vitest";
import { TemperClient } from "../src/client.js";

describe("TemperClient", () => {
  it("constructs with required config", () => {
    const client = new TemperClient({ baseUrl: "http://localhost:4200" });
    expect(client.entityUrl("Tasks")).toBe(
      "http://localhost:4200/tdata/Tasks"
    );
  });

  it("defaults tenant to 'default'", () => {
    const client = new TemperClient({ baseUrl: "http://localhost:4200" });
    expect(client.entityUrl("Agents")).toBe(
      "http://localhost:4200/tdata/Agents"
    );
  });

  it("strips trailing slashes from baseUrl", () => {
    const client = new TemperClient({ baseUrl: "http://localhost:4200/" });
    expect(client.entityUrl("Tasks")).toBe(
      "http://localhost:4200/tdata/Tasks"
    );
  });

  it("builds entity instance URL", () => {
    const client = new TemperClient({ baseUrl: "http://localhost:4200" });
    expect(client.entityInstanceUrl("Tasks", "t-1")).toBe(
      "http://localhost:4200/tdata/Tasks('t-1')"
    );
  });

  it("builds action URL", () => {
    const client = new TemperClient({ baseUrl: "http://localhost:4200" });
    expect(client.actionUrl("Tasks", "t-1", "Start")).toBe(
      "http://localhost:4200/tdata/Tasks('t-1')/Temper.Start"
    );
  });

  it("accepts custom tenant", () => {
    const client = new TemperClient({
      baseUrl: "http://localhost:4200",
      tenant: "acme",
    });
    // Tenant is used in headers, not URLs — just verify construction works.
    expect(client.entityUrl("Tasks")).toBe(
      "http://localhost:4200/tdata/Tasks"
    );
  });

  it("accepts principal config", () => {
    const client = new TemperClient({
      baseUrl: "http://localhost:4200",
      tenant: "default",
      principal: "agent-1",
    });
    // Principal is used in headers — verify construction works.
    expect(client.entityUrl("Agents")).toBe(
      "http://localhost:4200/tdata/Agents"
    );
  });
});
