import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { fetchSpecs, fetchSpecDetail, fetchEntities, runVerification, fetchEntityHistory, ApiError } from "@/lib/api";

const mockFetch = vi.fn();

beforeEach(() => {
  vi.stubGlobal("fetch", mockFetch);
});

afterEach(() => {
  vi.restoreAllMocks();
});

function jsonResponse(data: unknown, status = 200) {
  return Promise.resolve({
    ok: status >= 200 && status < 300,
    status,
    json: () => Promise.resolve(data),
  });
}

function errorResponse(status: number) {
  return Promise.resolve({
    ok: false,
    status,
    json: () => Promise.resolve({ error: "error" }),
  });
}

describe("fetchSpecs", () => {
  it("returns specs on success", async () => {
    const specs = [{ entity_type: "Ticket", states: ["Open"], actions: [], initial_state: "Open" }];
    mockFetch.mockReturnValue(jsonResponse({ specs, total: 1 }));
    const result = await fetchSpecs();
    expect(result).toEqual(specs);
    expect(mockFetch).toHaveBeenCalledWith("/observe/specs", { cache: "no-store" });
  });

  it("throws ApiError on 500", async () => {
    mockFetch.mockReturnValue(errorResponse(500));
    await expect(fetchSpecs()).rejects.toThrow(ApiError);
    await expect(fetchSpecs()).rejects.toThrow("500");
  }, 10000);

  it("throws on network error", async () => {
    mockFetch.mockRejectedValue(new TypeError("fetch failed"));
    await expect(fetchSpecs()).rejects.toThrow("fetch failed");
  }, 10000);
});

describe("fetchSpecDetail", () => {
  it("returns spec detail on success", async () => {
    const detail = { entity_type: "Ticket", states: ["Open"], initial_state: "Open", actions: [], invariants: [], state_variables: [] };
    mockFetch.mockReturnValue(jsonResponse(detail));
    const result = await fetchSpecDetail("Ticket");
    expect(result).toEqual(detail);
  });

  it("throws ApiError on 404", async () => {
    mockFetch.mockReturnValue(errorResponse(404));
    await expect(fetchSpecDetail("Unknown")).rejects.toThrow(ApiError);
  });

  it("URL-encodes the entity name", async () => {
    mockFetch.mockReturnValue(jsonResponse({}));
    await fetchSpecDetail("My Entity");
    expect(mockFetch).toHaveBeenCalledWith("/observe/specs/My%20Entity", { cache: "no-store" });
  });
});

describe("fetchEntities", () => {
  it("returns entities on success", async () => {
    const entities = [{ entity_type: "Ticket", entity_id: "TKT-001", actor_status: "active" }];
    mockFetch.mockReturnValue(jsonResponse({ entities, total: 1 }));
    const result = await fetchEntities();
    expect(result).toEqual(entities);
  });

  it("throws ApiError on server error", async () => {
    mockFetch.mockReturnValue(errorResponse(503));
    await expect(fetchEntities()).rejects.toThrow(ApiError);
  }, 10000);
});

describe("runVerification", () => {
  it("returns verification result on success", async () => {
    const result = { all_passed: true, levels: [] };
    mockFetch.mockReturnValue(jsonResponse(result));
    const data = await runVerification("Ticket");
    expect(data).toEqual(result);
    expect(mockFetch).toHaveBeenCalledWith(
      "/observe/verify/Ticket",
      { method: "POST", cache: "no-store" },
    );
  });

  it("throws ApiError on failure", async () => {
    mockFetch.mockReturnValue(errorResponse(500));
    await expect(runVerification("Ticket")).rejects.toThrow(ApiError);
  }, 10000);
});

describe("fetchEntityHistory", () => {
  it("returns entity history on success", async () => {
    const history = { entity_type: "Ticket", entity_id: "TKT-001", current_state: "Open", events: [] };
    mockFetch.mockReturnValue(jsonResponse(history));
    const result = await fetchEntityHistory("Ticket", "TKT-001");
    expect(result).toEqual(history);
  });

  it("throws ApiError on 404", async () => {
    mockFetch.mockReturnValue(errorResponse(404));
    await expect(fetchEntityHistory("Ticket", "UNKNOWN")).rejects.toThrow(ApiError);
  });

  it("URL-encodes entityType and entityId", async () => {
    mockFetch.mockReturnValue(jsonResponse({}));
    await fetchEntityHistory("My Type", "ID 001");
    expect(mockFetch).toHaveBeenCalledWith(
      "/observe/entities/My%20Type/ID%20001/history",
      { cache: "no-store" },
    );
  });
});

describe("fetchTrajectories", () => {
  it("returns trajectory data on success", async () => {
    const data = { total: 10, success_count: 8, error_count: 2, success_rate: 0.8, by_action: {}, failed_intents: [] };
    mockFetch.mockReturnValue(jsonResponse(data));
    const { fetchTrajectories } = await import("@/lib/api");
    const result = await fetchTrajectories();
    expect(result).toEqual(data);
    expect(mockFetch).toHaveBeenCalledWith("/observe/trajectories", { cache: "no-store" });
  });

  it("passes query params", async () => {
    mockFetch.mockReturnValue(jsonResponse({}));
    const { fetchTrajectories } = await import("@/lib/api");
    await fetchTrajectories({ entity_type: "Ticket", success: "true" });
    expect(mockFetch).toHaveBeenCalledWith(
      "/observe/trajectories?entity_type=Ticket&success=true",
      { cache: "no-store" },
    );
  });

  it("throws ApiError on failure", async () => {
    mockFetch.mockReturnValue(errorResponse(500));
    const { fetchTrajectories, ApiError: AE } = await import("@/lib/api");
    await expect(fetchTrajectories()).rejects.toThrow(AE);
  }, 10000);
});

describe("fetchEvolutionRecords", () => {
  it("returns records on success", async () => {
    const data = { records: [], total_observations: 0, total_problems: 0, total_analyses: 0, total_decisions: 0, total_insights: 0 };
    mockFetch.mockReturnValue(jsonResponse(data));
    const { fetchEvolutionRecords } = await import("@/lib/api");
    const result = await fetchEvolutionRecords();
    expect(result).toEqual(data);
    expect(mockFetch).toHaveBeenCalledWith("/observe/evolution/records", { cache: "no-store" });
  });

  it("passes filter params", async () => {
    mockFetch.mockReturnValue(jsonResponse({}));
    const { fetchEvolutionRecords } = await import("@/lib/api");
    await fetchEvolutionRecords({ record_type: "Observation", status: "active" });
    expect(mockFetch).toHaveBeenCalledWith(
      "/observe/evolution/records?record_type=Observation&status=active",
      { cache: "no-store" },
    );
  });

  it("throws ApiError on failure", async () => {
    mockFetch.mockReturnValue(errorResponse(500));
    const { fetchEvolutionRecords, ApiError: AE } = await import("@/lib/api");
    await expect(fetchEvolutionRecords()).rejects.toThrow(AE);
  }, 10000);
});

describe("fetchEvolutionInsights", () => {
  it("returns insights on success", async () => {
    const data = { insights: [], total: 0 };
    mockFetch.mockReturnValue(jsonResponse(data));
    const { fetchEvolutionInsights } = await import("@/lib/api");
    const result = await fetchEvolutionInsights();
    expect(result).toEqual(data);
  });

  it("throws ApiError on failure", async () => {
    mockFetch.mockReturnValue(errorResponse(500));
    const { fetchEvolutionInsights, ApiError: AE } = await import("@/lib/api");
    await expect(fetchEvolutionInsights()).rejects.toThrow(AE);
  }, 10000);
});

describe("triggerSentinelCheck", () => {
  it("sends POST and returns alerts on success", async () => {
    const data = { alerts_count: 0, alerts: [] };
    mockFetch.mockReturnValue(jsonResponse(data));
    const { triggerSentinelCheck } = await import("@/lib/api");
    const result = await triggerSentinelCheck();
    expect(result).toEqual(data);
    expect(mockFetch).toHaveBeenCalledWith("/api/evolution/sentinel/check", {
      method: "POST",
      cache: "no-store",
    });
  });

  it("throws ApiError on failure", async () => {
    mockFetch.mockReturnValue(errorResponse(500));
    const { triggerSentinelCheck, ApiError: AE } = await import("@/lib/api");
    await expect(triggerSentinelCheck()).rejects.toThrow(AE);
  }, 10000);
});

describe("fetchMetricsText", () => {
  it("returns text on success", async () => {
    mockFetch.mockReturnValue(
      Promise.resolve({
        ok: true,
        status: 200,
        text: () => Promise.resolve("# HELP transitions_total\ntransitions_total 42"),
      }),
    );
    const { fetchMetricsText } = await import("@/lib/api");
    const result = await fetchMetricsText();
    expect(result).toContain("transitions_total");
  });

  it("throws ApiError on failure", async () => {
    mockFetch.mockReturnValue(errorResponse(500));
    const { fetchMetricsText, ApiError: AE } = await import("@/lib/api");
    await expect(fetchMetricsText()).rejects.toThrow(AE);
  }, 10000);
});

describe("ApiError", () => {
  it("has correct name and status", () => {
    const err = new ApiError("test", 404);
    expect(err.name).toBe("ApiError");
    expect(err.status).toBe(404);
    expect(err.message).toBe("test");
  });
});
