import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";
import ActivityPage from "@/app/activity/page";

vi.mock("@/lib/api", () => ({
  fetchTrajectories: vi.fn(),
  subscribeEntityEvents: vi.fn().mockReturnValue(() => {}),
}));

vi.mock("@/lib/hooks", () => ({
  usePolling: vi.fn(),
  useRelativeTime: vi.fn(),
}));

vi.mock("next/link", () => ({
  default: ({ children, href, ...props }: { children: React.ReactNode; href: string; [key: string]: unknown }) => (
    <a href={href} {...props}>{children}</a>
  ),
}));

import { fetchTrajectories } from "@/lib/api";
import { usePolling, useRelativeTime } from "@/lib/hooks";

const mockFetchTrajectories = vi.mocked(fetchTrajectories);
const mockUsePolling = vi.mocked(usePolling);
const mockUseRelativeTime = vi.mocked(useRelativeTime);

const sampleTrajectoryData = {
  total: 42,
  success_count: 38,
  error_count: 4,
  success_rate: 0.905,
  by_action: {
    create: { total: 20, success: 19, error: 1 },
    close: { total: 22, success: 19, error: 3 },
  },
  failed_intents: [
    {
      timestamp: "2026-02-18T10:00:00Z",
      tenant: "default",
      entity_type: "Ticket",
      entity_id: "TKT-001",
      action: "close",
      from_status: "Open",
      error: "Guard rejected: not assignee",
    },
    {
      timestamp: "2026-02-18T10:01:00Z",
      tenant: "default",
      entity_type: "",
      entity_id: "",
      action: "assign to me",
      from_status: null,
      error: "No matching action found",
    },
  ],
};

function setPollingReturn(data: unknown, opts?: { error?: string }) {
  mockUsePolling.mockReturnValue({
    data: data as never,
    error: opts?.error ?? null,
    loading: false,
    lastUpdated: data ? new Date() : null,
    refresh: vi.fn(),
  });
}

beforeEach(() => {
  vi.clearAllMocks();
  mockUseRelativeTime.mockReturnValue("5s ago");
  setPollingReturn(sampleTrajectoryData);
});

describe("Activity page", () => {
  it("shows loading skeleton initially", () => {
    mockFetchTrajectories.mockReturnValue(new Promise(() => {}));
    setPollingReturn(null);
    const { container } = render(<ActivityPage />);
    expect(container.querySelector(".animate-pulse")).toBeTruthy();
  });

  it("renders trajectory stats on success", async () => {
    mockFetchTrajectories.mockResolvedValue(sampleTrajectoryData);
    render(<ActivityPage />);
    await waitFor(() => {
      expect(screen.getByText("Activity")).toBeInTheDocument();
    });
    expect(screen.getByText("Total Transitions")).toBeInTheDocument();
    expect(screen.getByText("42")).toBeInTheDocument();
    expect(screen.getByText("91%")).toBeInTheDocument();
    expect(screen.getByText("4")).toBeInTheDocument();
  });

  it("renders action breakdown table", async () => {
    mockFetchTrajectories.mockResolvedValue(sampleTrajectoryData);
    render(<ActivityPage />);
    await waitFor(() => {
      expect(screen.getByText("Action Breakdown")).toBeInTheDocument();
    });
    expect(screen.getByText("create")).toBeInTheDocument();
    // "close" appears in both action breakdown and failed intents
    expect(screen.getAllByText("close").length).toBeGreaterThanOrEqual(1);
  });

  it("renders failed intents list", async () => {
    mockFetchTrajectories.mockResolvedValue(sampleTrajectoryData);
    render(<ActivityPage />);
    await waitFor(() => {
      expect(screen.getByText("Failed Intents")).toBeInTheDocument();
    });
    expect(screen.getByText("Guard rejected: not assignee")).toBeInTheDocument();
  });

  it("shows unmet intent badge for entries with empty entity_type", async () => {
    mockFetchTrajectories.mockResolvedValue(sampleTrajectoryData);
    render(<ActivityPage />);
    await waitFor(() => {
      expect(screen.getByText("unmet")).toBeInTheDocument();
    });
    expect(screen.getByText("assign to me")).toBeInTheDocument();
  });

  it("shows error state when API fails", async () => {
    mockFetchTrajectories.mockRejectedValue(new Error("Network error"));
    setPollingReturn(null);
    render(<ActivityPage />);
    await waitFor(() => {
      expect(screen.getByText("Cannot load activity")).toBeInTheDocument();
    });
    expect(screen.getByText("Retry")).toBeInTheDocument();
  });
});
