import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, screen, waitFor, fireEvent } from "@testing-library/react";
import EvolutionPage from "@/app/(observe)/evolution/page";

vi.mock("@/lib/api", async (importOriginal) => {
  const actual = await importOriginal();
  return {
    ...actual,
    fetchEvolutionRecords: vi.fn(),
    fetchEvolutionInsights: vi.fn(),
    triggerSentinelCheck: vi.fn(),
    fetchUnmetIntents: vi.fn().mockResolvedValue({ open_count: 0, intents: [] }),
    fetchFeatureRequests: vi.fn().mockResolvedValue([]),
    subscribeEvolutionEvents: vi.fn().mockReturnValue(() => {}),
  };
});

vi.mock("@/lib/hooks", () => ({
  usePolling: vi.fn(),
  useRelativeTime: vi.fn(),
}));

vi.mock("next/link", () => ({
  default: ({ children, href, ...props }: { children: React.ReactNode; href: string; [key: string]: unknown }) => (
    <a href={href} {...props}>{children}</a>
  ),
}));

import { fetchEvolutionRecords, triggerSentinelCheck } from "@/lib/api";
import { usePolling, useRelativeTime } from "@/lib/hooks";

const mockFetchRecords = vi.mocked(fetchEvolutionRecords);
const mockTriggerSentinel = vi.mocked(triggerSentinelCheck);
const mockUsePolling = vi.mocked(usePolling);
const mockUseRelativeTime = vi.mocked(useRelativeTime);

const sampleRecords = {
  records: [
    { id: "obs-001", record_type: "Observation", status: "active", source: "user_intent", classification: "unmet_intent" },
    { id: "ins-001", record_type: "Insight", status: "active", recommendation: "Add assign action", category: "capability_gap" },
  ],
  total_observations: 5,
  total_problems: 2,
  total_analyses: 1,
  total_decisions: 0,
  total_insights: 3,
};

const sampleInsights = {
  insights: [
    {
      id: "ins-001",
      category: "capability_gap",
      priority_score: 0.85,
      recommendation: "Add assign action to Ticket entity",
      signal: { intent: "assign", volume: 12, success_rate: 0.0, trend: "growing" },
      status: "active",
      timestamp: "2026-02-18T10:00:00Z",
    },
  ],
  total: 1,
};

const sampleUnmetIntents = { intents: [], open_count: 0, resolved_count: 0 };

function setPollingReturns(records: unknown, insights: unknown, unmet?: unknown) {
  let callIndex = 0;
  const results = [
    {
      data: records as never,
      error: null,
      loading: false,
      lastUpdated: records ? new Date() : null,
      refresh: vi.fn(),
    },
    {
      data: insights as never,
      error: null,
      loading: false,
      lastUpdated: insights ? new Date() : null,
      refresh: vi.fn(),
    },
    {
      data: (unmet ?? sampleUnmetIntents) as never,
      error: null,
      loading: false,
      lastUpdated: new Date(),
      refresh: vi.fn(),
    },
  ];
  mockUsePolling.mockImplementation(() => {
    const result = results[callIndex % results.length];
    callIndex++;
    return result;
  });
}

beforeEach(() => {
  vi.clearAllMocks();
  mockUseRelativeTime.mockReturnValue("5s ago");
  setPollingReturns(sampleRecords, sampleInsights);
});

describe("Evolution page", () => {
  it("shows loading skeleton initially", () => {
    mockFetchRecords.mockReturnValue(new Promise(() => {}));
    setPollingReturns(null, null);
    const { container } = render(<EvolutionPage />);
    expect(container.querySelector(".animate-pulse")).toBeTruthy();
  });

  it("renders record summary counts", async () => {
    mockFetchRecords.mockResolvedValue(sampleRecords);
    render(<EvolutionPage />);
    await waitFor(() => {
      expect(screen.getByText("Evolution")).toBeInTheDocument();
    });
    // "Observations" and "Insights" appear in both summary cards and tab buttons
    expect(screen.getAllByText("Observations").length).toBeGreaterThanOrEqual(1);
    expect(screen.getByText("5")).toBeInTheDocument();
    expect(screen.getAllByText("Problems").length).toBeGreaterThanOrEqual(1);
    expect(screen.getByText("2")).toBeInTheDocument();
    expect(screen.getAllByText("Insights").length).toBeGreaterThanOrEqual(1);
    expect(screen.getByText("3")).toBeInTheDocument();
  });

  it("renders insights with priority badges", async () => {
    mockFetchRecords.mockResolvedValue(sampleRecords);
    render(<EvolutionPage />);
    await waitFor(() => {
      expect(screen.getByText("Add assign action to Ticket entity")).toBeInTheDocument();
    });
    expect(screen.getByText("high")).toBeInTheDocument();
    expect(screen.getByText("capability_gap")).toBeInTheDocument();
  });

  it("sentinel check button triggers POST and shows alerts", async () => {
    mockFetchRecords.mockResolvedValue(sampleRecords);
    mockTriggerSentinel.mockResolvedValue({
      alerts_count: 1,
      alerts: [
        { rule: "high_error_rate", record_id: "obs-001", source: "Ticket", classification: "Observation", threshold: 0.1, observed: 0.25 },
      ],
    });

    render(<EvolutionPage />);
    await waitFor(() => {
      expect(screen.getByText("Run Health Check")).toBeInTheDocument();
    });

    fireEvent.click(screen.getByText("Run Health Check"));

    await waitFor(() => {
      expect(mockTriggerSentinel).toHaveBeenCalled();
    });

    await waitFor(() => {
      expect(screen.getByText("high_error_rate")).toBeInTheDocument();
    });
  });

  it("shows empty state when no records", async () => {
    mockFetchRecords.mockResolvedValue(sampleRecords);
    setPollingReturns(
      { records: [], total_observations: 0, total_problems: 0, total_analyses: 0, total_decisions: 0, total_insights: 0 },
      { insights: [], total: 0 },
    );
    render(<EvolutionPage />);
    await waitFor(() => {
      expect(screen.getByText("No records found.")).toBeInTheDocument();
    });
  });

  it("shows error state when API fails", async () => {
    mockFetchRecords.mockRejectedValue(new Error("Server error"));
    setPollingReturns(null, null);
    render(<EvolutionPage />);
    await waitFor(() => {
      expect(screen.getByText("Cannot load evolution data")).toBeInTheDocument();
    });
    expect(screen.getByText("Retry")).toBeInTheDocument();
  });
});
