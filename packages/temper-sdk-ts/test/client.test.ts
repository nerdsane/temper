import { describe, it, expect, vi, afterEach } from "vitest";
import { TemperClient } from "../src/client.js";

type Page = { value?: unknown; "@odata.nextLink"?: unknown };

/** Mock global fetch with a scripted sequence of OData pages. Each fake
 *  Response exposes `url` (absolute) so relative-nextLink resolution is
 *  exercised the way a real fetch behaves. Records the requested URLs. */
function stubPages(pages: Page[], opts?: { headerSink?: Record<string, string>[] }) {
  const calls: string[] = [];
  let i = 0;
  vi.stubGlobal("fetch", async (input: unknown, init?: { headers?: Record<string, string> }) => {
    const requested = String(input);
    calls.push(requested);
    if (opts?.headerSink) opts.headerSink.push({ ...(init?.headers ?? {}) });
    const page = pages[Math.min(i++, pages.length - 1)];
    const absUrl = requested.startsWith("http") ? requested : `https://server${requested}`;
    return {
      ok: true,
      status: 200,
      url: absUrl,
      json: async () => page,
    } as unknown as Response;
  });
  return calls;
}

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

describe("TemperClient.list pagination", () => {
  afterEach(() => vi.unstubAllGlobals());

  it("follows @odata.nextLink to completion and preserves /tdata", async () => {
    const calls = stubPages([
      { value: [{ id: 1 }, { id: 2 }], "@odata.nextLink": "Tasks?$skiptoken=a" },
      { value: [{ id: 3 }], "@odata.nextLink": "Tasks?$skiptoken=b" },
      { value: [{ id: 4 }] },
    ]);
    const client = new TemperClient({ baseUrl: "http://srv:4200" });
    const rows = await client.list("Tasks");
    expect(rows).toEqual([{ id: 1 }, { id: 2 }, { id: 3 }, { id: 4 }]);
    expect(calls).toHaveLength(3);
    expect(calls[1]).toBe("http://srv:4200/tdata/Tasks?$skiptoken=a");
    expect(calls[2]).toBe("http://srv:4200/tdata/Tasks?$skiptoken=b");
  });

  it("resolves relative nextLinks even with a relative baseUrl (browser)", async () => {
    const calls = stubPages([
      { value: [{ id: 1 }], "@odata.nextLink": "Tasks?$skiptoken=a" },
      { value: [{ id: 2 }] },
    ]);
    const client = new TemperClient({ baseUrl: "/api" });
    const rows = await client.list("Tasks");
    expect(rows).toEqual([{ id: 1 }, { id: 2 }]);
    // Page 2 resolved against the absolute response URL, keeping /api/tdata.
    expect(calls[1]).toBe("https://server/api/tdata/Tasks?$skiptoken=a");
  });

  it("throws on a repeated nextLink instead of looping + duplicating", async () => {
    stubPages([{ value: [{ id: 1 }], "@odata.nextLink": "Tasks?$skiptoken=same" }]);
    const client = new TemperClient({ baseUrl: "http://srv:4200" });
    await expect(client.list("Tasks")).rejects.toThrow(/looped on a repeated nextLink/);
  });

  it("throws when a page omits the value array (never silently ends)", async () => {
    stubPages([
      { value: [{ id: 1 }], "@odata.nextLink": "Tasks?$skiptoken=a" },
      {} as Page,
    ]);
    const client = new TemperClient({ baseUrl: "http://srv:4200" });
    await expect(client.list("Tasks")).rejects.toThrow(/without a value array/);
  });

  it("throws on an invalid nextLink type", async () => {
    stubPages([{ value: [{ id: 1 }], "@odata.nextLink": 42 }]);
    const client = new TemperClient({ baseUrl: "http://srv:4200" });
    await expect(client.list("Tasks")).rejects.toThrow(/invalid @odata.nextLink/);
  });

  it("rejects invalid page sizes (0, negative, fractional, NaN)", async () => {
    const client = new TemperClient({ baseUrl: "http://srv:4200" });
    for (const bad of [0, -1, 1.5, NaN, Infinity]) {
      await expect(client.list("Tasks", { pageSize: bad })).rejects.toThrow(/positive integer/);
      await expect(client.listPage("Tasks", { pageSize: bad })).rejects.toThrow(/positive integer/);
    }
  });

  it("listPage validates the envelope and does not paginate", async () => {
    const calls = stubPages([
      { value: [{ id: 1 }], "@odata.nextLink": "Tasks?$skiptoken=a" },
      { value: [{ id: 2 }] },
    ]);
    const client = new TemperClient({ baseUrl: "http://srv:4200" });
    const rows = await client.listPage("Tasks");
    expect(rows).toEqual([{ id: 1 }]);
    expect(calls).toHaveLength(1);
  });

  it("listPage throws when value is not an array", async () => {
    stubPages([{ value: "nope" }]);
    const client = new TemperClient({ baseUrl: "http://srv:4200" });
    await expect(client.listPage("Tasks")).rejects.toThrow(/without a value array/);
  });

  it("forwards the configured principal on read requests", async () => {
    const headerSink: Record<string, string>[] = [];
    stubPages([{ value: [{ id: 1 }] }], { headerSink });
    const client = new TemperClient({ baseUrl: "http://srv:4200", principal: "agent-1" });
    await client.list("Tasks");
    expect(headerSink[0]["x-temper-principal-id"]).toBe("agent-1");
    expect(headerSink[0]["x-temper-principal-kind"]).toBe("Agent");
  });
});
