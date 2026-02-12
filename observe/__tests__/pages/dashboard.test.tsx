import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, screen, waitFor, fireEvent } from "@testing-library/react";
import Dashboard from "@/app/page";

vi.mock("@/lib/api", () => ({
  fetchSpecs: vi.fn(),
  fetchEntities: vi.fn(),
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

import { fetchSpecs } from "@/lib/api";
import { usePolling, useRelativeTime } from "@/lib/hooks";

const mockFetchSpecs = vi.mocked(fetchSpecs);
const mockUsePolling = vi.mocked(usePolling);
const mockUseRelativeTime = vi.mocked(useRelativeTime);

function setPollingReturn(data: unknown[] | null, opts?: { error?: string; loading?: boolean }) {
  mockUsePolling.mockReturnValue({
    data: data as never,
    error: opts?.error ?? null,
    loading: opts?.loading ?? false,
    lastUpdated: data ? new Date() : null,
    refresh: vi.fn(),
  });
}

beforeEach(() => {
  vi.clearAllMocks();
  mockUseRelativeTime.mockReturnValue("5s ago");
  setPollingReturn([]);
});

describe("Dashboard page", () => {
  it("shows loading skeleton initially", () => {
    mockFetchSpecs.mockReturnValue(new Promise(() => {}));
    setPollingReturn(null, { loading: true });
    const { container } = render(<Dashboard />);
    expect(container.querySelector(".animate-pulse")).toBeTruthy();
  });

  it("shows empty state when no data", async () => {
    mockFetchSpecs.mockResolvedValue([]);
    setPollingReturn([]);
    render(<Dashboard />);
    await waitFor(() => {
      expect(screen.getByText("No specs loaded")).toBeInTheDocument();
    });
  });

  it("renders specs and entities on success", async () => {
    mockFetchSpecs.mockResolvedValue([
      { entity_type: "Ticket", states: ["Open", "Closed"], actions: ["close"], initial_state: "Open" },
    ]);
    setPollingReturn([
      { entity_type: "Ticket", entity_id: "TKT-001", actor_status: "active", current_state: "Open" },
    ]);

    render(<Dashboard />);

    await waitFor(() => {
      expect(screen.getByText("Dashboard")).toBeInTheDocument();
    });

    expect(screen.getAllByText("Ticket").length).toBeGreaterThanOrEqual(1);
    expect(screen.getByText("TKT-001")).toBeInTheDocument();
    expect(screen.getByText("Inspect")).toBeInTheDocument();
    expect(screen.getByText("Loaded Specs")).toBeInTheDocument();
    expect(screen.getByText("Active Entities")).toBeInTheDocument();
  });

  it("shows error state when API fails", async () => {
    mockFetchSpecs.mockRejectedValue(new Error("Network error"));
    setPollingReturn([]);
    render(<Dashboard />);
    await waitFor(() => {
      expect(screen.getByText("Cannot load dashboard")).toBeInTheDocument();
    });
    expect(screen.getByText("Retry")).toBeInTheDocument();
  });

  it("filters entities by search query", async () => {
    mockFetchSpecs.mockResolvedValue([
      { entity_type: "Ticket", states: ["Open"], actions: [], initial_state: "Open" },
    ]);
    setPollingReturn([
      { entity_type: "Ticket", entity_id: "TKT-001", actor_status: "active", current_state: "Open" },
      { entity_type: "Ticket", entity_id: "TKT-002", actor_status: "active", current_state: "Closed" },
    ]);

    render(<Dashboard />);

    await waitFor(() => {
      expect(screen.getByText("TKT-001")).toBeInTheDocument();
    });

    const searchInput = screen.getByPlaceholderText("Search by ID...");
    fireEvent.change(searchInput, { target: { value: "002" } });

    expect(screen.queryByText("TKT-001")).not.toBeInTheDocument();
    expect(screen.getByText("TKT-002")).toBeInTheDocument();
  });

  it("shows clear filters button when filter is active", async () => {
    mockFetchSpecs.mockResolvedValue([
      { entity_type: "Ticket", states: ["Open"], actions: [], initial_state: "Open" },
    ]);
    setPollingReturn([
      { entity_type: "Ticket", entity_id: "TKT-001", actor_status: "active", current_state: "Open" },
    ]);

    render(<Dashboard />);

    await waitFor(() => {
      expect(screen.getByText("TKT-001")).toBeInTheDocument();
    });

    expect(screen.queryByText("Clear filters")).not.toBeInTheDocument();

    const searchInput = screen.getByPlaceholderText("Search by ID...");
    fireEvent.change(searchInput, { target: { value: "test" } });

    expect(screen.getByText("Clear filters")).toBeInTheDocument();
  });

  it("shows polling error indicator", async () => {
    mockFetchSpecs.mockResolvedValue([
      { entity_type: "Ticket", states: ["Open"], actions: [], initial_state: "Open" },
    ]);
    setPollingReturn(
      [{ entity_type: "Ticket", entity_id: "TKT-001", actor_status: "active", current_state: "Open" }],
      { error: "Connection lost" },
    );

    render(<Dashboard />);

    await waitFor(() => {
      expect(screen.getByText("Polling error")).toBeInTheDocument();
    });
  });

  it("shows last updated time", async () => {
    mockUseRelativeTime.mockReturnValue("10s ago");
    mockFetchSpecs.mockResolvedValue([
      { entity_type: "Ticket", states: ["Open"], actions: [], initial_state: "Open" },
    ]);
    setPollingReturn([
      { entity_type: "Ticket", entity_id: "TKT-001", actor_status: "active", current_state: "Open" },
    ]);

    render(<Dashboard />);

    await waitFor(() => {
      expect(screen.getByText("Updated 10s ago")).toBeInTheDocument();
    });
  });
});
