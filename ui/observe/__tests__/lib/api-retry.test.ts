import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { fetchSpecs, checkConnection } from "@/lib/api";

const mockFetch = vi.fn();

beforeEach(() => {
  mockFetch.mockClear();
  vi.stubGlobal("fetch", mockFetch);
});

afterEach(() => {
  vi.restoreAllMocks();
});

function jsonResponse(data: unknown, status = 200) {
  return {
    ok: status >= 200 && status < 300,
    status,
    json: () => Promise.resolve(data),
  };
}

describe("fetchWithRetry (via fetchSpecs)", () => {
  it("does not retry on 200", async () => {
    mockFetch.mockResolvedValue(jsonResponse({ specs: [], total: 0 }));
    const result = await fetchSpecs();
    expect(result).toEqual([]);
    expect(mockFetch).toHaveBeenCalledTimes(1);
  });

  it("does not retry on 404 (non-transient)", async () => {
    mockFetch.mockResolvedValue(jsonResponse(null, 404));
    await expect(fetchSpecs()).rejects.toThrow("404");
    expect(mockFetch).toHaveBeenCalledTimes(1);
  });

  it("retries once on 500", async () => {
    mockFetch
      .mockResolvedValueOnce(jsonResponse(null, 500))
      .mockResolvedValueOnce(jsonResponse({ specs: ["retried"], total: 1 }));

    const result = await fetchSpecs();
    expect(result).toEqual(["retried"]);
    expect(mockFetch).toHaveBeenCalledTimes(2);
  }, 10000);

  it("retries once on 429", async () => {
    mockFetch
      .mockResolvedValueOnce(jsonResponse(null, 429))
      .mockResolvedValueOnce(jsonResponse({ specs: ["ok"], total: 1 }));

    const result = await fetchSpecs();
    expect(result).toEqual(["ok"]);
    expect(mockFetch).toHaveBeenCalledTimes(2);
  }, 10000);

  it("retries once on network error", async () => {
    mockFetch
      .mockRejectedValueOnce(new TypeError("fetch failed"))
      .mockResolvedValueOnce(jsonResponse({ specs: ["recovered"], total: 1 }));

    const result = await fetchSpecs();
    expect(result).toEqual(["recovered"]);
    expect(mockFetch).toHaveBeenCalledTimes(2);
  }, 10000);

  it("throws if retry also fails on network error", async () => {
    mockFetch.mockRejectedValue(new TypeError("fetch failed"));
    await expect(fetchSpecs()).rejects.toThrow("fetch failed");
    expect(mockFetch).toHaveBeenCalledTimes(2);
  }, 10000);
});

describe("checkConnection", () => {
  it("returns true when server is reachable", async () => {
    mockFetch.mockResolvedValue(jsonResponse([]));
    const result = await checkConnection();
    expect(result).toBe(true);
  });

  it("returns false when server returns error", async () => {
    mockFetch.mockResolvedValue(jsonResponse(null, 500));
    const result = await checkConnection();
    expect(result).toBe(false);
  });

  it("returns false on network error", async () => {
    mockFetch.mockRejectedValue(new TypeError("fetch failed"));
    const result = await checkConnection();
    expect(result).toBe(false);
  });
});
