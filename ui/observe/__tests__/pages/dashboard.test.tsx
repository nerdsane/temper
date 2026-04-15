import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";
import Dashboard from "@/app/(observe)/dashboard/page";

vi.mock("@/lib/api", () => ({
  fetchSpecs: vi.fn(),
  fetchEntities: vi.fn(),
  fetchVerificationStatus: vi.fn().mockResolvedValue({ pending: 0, running: 0, passed: 0, failed: 0, partial: 0, entities: [] }),
  subscribeDesignTimeEvents: vi.fn().mockReturnValue(() => {}),
  subscribeEntityEvents: vi.fn().mockReturnValue(() => {}),
}));

vi.mock("@/lib/hooks", () => ({
  useSSERefresh: vi.fn(),
  useRelativeTime: vi.fn(),
}));

vi.mock("@/lib/sse-context", () => ({
  useSSERefreshSubscribe: vi.fn(),
  useSSEConnected: vi.fn().mockReturnValue(true),
}));

vi.mock("next/link", () => ({
  default: ({ children, href, ...props }: { children: React.ReactNode; href: string; [key: string]: unknown }) => (
    <a href={href} {...props}>{children}</a>
  ),
}));

vi.mock("next/navigation", () => ({
  useRouter: () => ({ push: vi.fn(), replace: vi.fn(), back: vi.fn(), forward: vi.fn(), prefetch: vi.fn(), refresh: vi.fn() }),
}));

import { fetchSpecs } from "@/lib/api";
import { useSSERefresh, useRelativeTime } from "@/lib/hooks";

const mockFetchSpecs = vi.mocked(fetchSpecs);
const mockUseSSERefresh = vi.mocked(useSSERefresh);
const mockUseRelativeTime = vi.mocked(useRelativeTime);

const emptyVerifyStatus = { pending: 0, running: 0, passed: 0, failed: 0, partial: 0, entities: [] };

/**
 * Set up the 3 useSSERefresh calls: specs, entities, verification status.
 * Uses mockImplementation with a counter that cycles through the 3 return values.
 */
function setSSERefreshReturns(
  specs: unknown[] | null,
  entities: unknown[] | null,
  opts?: { error?: string; loading?: boolean },
) {
  let callIndex = 0;
  const results = [
    {
      data: specs as never,
      error: opts?.error ?? null,
      loading: opts?.loading ?? false,
      lastUpdated: specs ? new Date() : null,
      refresh: vi.fn(),
    },
    {
      data: entities as never,
      error: opts?.error ?? null,
      loading: opts?.loading ?? false,
      lastUpdated: entities ? new Date() : null,
      refresh: vi.fn(),
    },
    {
      data: emptyVerifyStatus as never,
      error: null,
      loading: false,
      lastUpdated: null,
      refresh: vi.fn(),
    },
  ];
  mockUseSSERefresh.mockImplementation(() => {
    const result = results[callIndex % 3];
    callIndex++;
    return result;
  });
}

beforeEach(() => {
  vi.clearAllMocks();
  mockUseRelativeTime.mockReturnValue("5s ago");
  setSSERefreshReturns([], []);
});

describe("Dashboard page", () => {
  it("shows loading skeleton initially", () => {
    mockFetchSpecs.mockReturnValue(new Promise(() => {}));
    setSSERefreshReturns(null, null, { loading: true });
    const { container } = render(<Dashboard />);
    expect(container.querySelector(".animate-pulse")).toBeTruthy();
  });

  it("shows empty state when no data", async () => {
    mockFetchSpecs.mockResolvedValue([]);
    setSSERefreshReturns([], []);
    render(<Dashboard />);
    await waitFor(() => {
      expect(screen.getByText("No specs loaded")).toBeInTheDocument();
    });
  });

  it("renders specs on success", async () => {
    mockFetchSpecs.mockResolvedValue([
      { entity_type: "Ticket", states: ["Open", "Closed"], actions: ["close"], initial_state: "Open" },
    ]);
    setSSERefreshReturns(
      [{ entity_type: "Ticket", states: ["Open", "Closed"], actions: ["close"], initial_state: "Open" }],
      [{ entity_type: "Ticket", entity_id: "TKT-001", actor_status: "active", current_state: "Open" }],
    );

    render(<Dashboard />);

    await waitFor(() => {
      expect(screen.getByText("Dashboard")).toBeInTheDocument();
    });

    expect(screen.getAllByText("Ticket").length).toBeGreaterThanOrEqual(1);
    expect(screen.getByText("Loaded Specs")).toBeInTheDocument();
  });

  it("shows error state when API fails", async () => {
    mockFetchSpecs.mockRejectedValue(new Error("Network error"));
    setSSERefreshReturns([], []);
    render(<Dashboard />);
    await waitFor(() => {
      expect(screen.getByText("Cannot load dashboard")).toBeInTheDocument();
    });
    expect(screen.getByText("Retry")).toBeInTheDocument();
  });

  it("shows last updated time", async () => {
    mockUseRelativeTime.mockReturnValue("10s ago");
    mockFetchSpecs.mockResolvedValue([
      { entity_type: "Ticket", states: ["Open"], actions: [], initial_state: "Open" },
    ]);
    setSSERefreshReturns(
      [{ entity_type: "Ticket", states: ["Open"], actions: [], initial_state: "Open" }],
      [{ entity_type: "Ticket", entity_id: "TKT-001", actor_status: "active", current_state: "Open" }],
    );

    render(<Dashboard />);

    await waitFor(() => {
      expect(screen.getByText("Updated 10s ago")).toBeInTheDocument();
    });
  });
});
