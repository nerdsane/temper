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
    mockFetch.mockReturnValue(jsonResponse(specs));
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
    mockFetch.mockReturnValue(jsonResponse(entities));
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

describe("ApiError", () => {
  it("has correct name and status", () => {
    const err = new ApiError("test", 404);
    expect(err.name).toBe("ApiError");
    expect(err.status).toBe(404);
    expect(err.message).toBe("test");
  });
});
