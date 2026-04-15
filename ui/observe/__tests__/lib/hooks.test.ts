import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { renderHook, act } from "@testing-library/react";

// Mock the SSE context before importing hooks
vi.mock("@/lib/sse-context", () => {
  let capturedCallback: (() => void) | null = null;
  return {
    useSSERefreshSubscribe: vi.fn((kinds: string[], callback: () => void) => {
      capturedCallback = callback;
    }),
    useSSEConnected: vi.fn().mockReturnValue(true),
    // Helper for tests to trigger the captured callback
    __triggerSSE: () => { if (capturedCallback) capturedCallback(); },
    __resetCapture: () => { capturedCallback = null; },
  };
});

import { useSSERefresh, useRelativeTime } from "@/lib/hooks";

// Access the test helpers
const sseContextMock = await import("@/lib/sse-context") as unknown as {
  useSSERefreshSubscribe: ReturnType<typeof vi.fn>;
  __triggerSSE: () => void;
  __resetCapture: () => void;
};

describe("useSSERefresh", () => {
  beforeEach(() => {
    vi.useFakeTimers();
    sseContextMock.__resetCapture();
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  it("fetches data on mount", async () => {
    const fetcher = vi.fn().mockResolvedValue(["a", "b"]);
    const { result } = renderHook(() =>
      useSSERefresh({ fetcher, sseKinds: ["Test"] }),
    );

    expect(result.current.loading).toBe(true);

    // Flush the initial fetch
    await act(async () => {
      await vi.advanceTimersByTimeAsync(0);
    });

    expect(result.current.loading).toBe(false);
    expect(result.current.data).toEqual(["a", "b"]);
    expect(result.current.error).toBeNull();
    expect(result.current.lastUpdated).toBeInstanceOf(Date);
    expect(fetcher).toHaveBeenCalledTimes(1);
  });

  it("refetches when SSE callback is triggered", async () => {
    let callCount = 0;
    const fetcher = vi.fn().mockImplementation(() => {
      callCount++;
      return Promise.resolve([`call-${callCount}`]);
    });
    const { result } = renderHook(() =>
      useSSERefresh({ fetcher, sseKinds: ["Test"] }),
    );

    // Initial fetch
    await act(async () => {
      await vi.advanceTimersByTimeAsync(0);
    });
    expect(result.current.data).toEqual(["call-1"]);

    // Trigger SSE callback
    await act(async () => {
      sseContextMock.__triggerSSE();
      await vi.advanceTimersByTimeAsync(0);
    });
    expect(result.current.data).toEqual(["call-2"]);
  });

  it("does not fetch when disabled", async () => {
    const fetcher = vi.fn().mockResolvedValue([]);
    const { result } = renderHook(() =>
      useSSERefresh({ fetcher, sseKinds: ["Test"], enabled: false }),
    );

    await act(async () => {
      await vi.advanceTimersByTimeAsync(10000);
    });

    expect(fetcher).not.toHaveBeenCalled();
    expect(result.current.loading).toBe(true);
  });

  it("sets error on fetch failure", async () => {
    const fetcher = vi.fn().mockRejectedValue(new Error("API down"));
    const { result } = renderHook(() =>
      useSSERefresh({ fetcher, sseKinds: ["Test"] }),
    );

    await act(async () => {
      await vi.advanceTimersByTimeAsync(0);
    });

    expect(result.current.loading).toBe(false);
    expect(result.current.error).toBe("API down");
    expect(result.current.data).toBeNull();
  });

  it("provides a refresh function", async () => {
    let callCount = 0;
    const fetcher = vi.fn().mockImplementation(() => {
      callCount++;
      return Promise.resolve([`call-${callCount}`]);
    });
    const { result } = renderHook(() =>
      useSSERefresh({ fetcher, sseKinds: ["Test"] }),
    );

    await act(async () => {
      await vi.advanceTimersByTimeAsync(0);
    });
    expect(result.current.data).toEqual(["call-1"]);

    await act(async () => {
      await result.current.refresh();
    });
    expect(result.current.data).toEqual(["call-2"]);
  });
});

describe("useRelativeTime", () => {
  it("returns empty string for null date", () => {
    const { result } = renderHook(() => useRelativeTime(null));
    expect(result.current).toBe("");
  });

  it("returns 'just now' for recent dates", () => {
    const now = new Date();
    const { result } = renderHook(() => useRelativeTime(now));
    expect(result.current).toBe("just now");
  });

  it("returns seconds ago", () => {
    const date = new Date(Date.now() - 15000);
    const { result } = renderHook(() => useRelativeTime(date));
    expect(result.current).toMatch(/^\d+s ago$/);
  });

  it("returns minutes ago", () => {
    const date = new Date(Date.now() - 120000);
    const { result } = renderHook(() => useRelativeTime(date));
    expect(result.current).toMatch(/^\d+m ago$/);
  });
});
