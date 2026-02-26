import { describe, it, expect } from "vitest";
import { render, screen } from "@testing-library/react";
import EntityTimeline from "@/components/EntityTimeline";
import type { EntityEvent } from "@/lib/types";

const mockEvents: EntityEvent[] = [
  {
    timestamp: "2026-02-10T09:00:00Z",
    action: "create",
    from_state: "(init)",
    to_state: "Open",
    actor: "system",
  },
  {
    timestamp: "2026-02-10T10:30:00Z",
    action: "start_work",
    from_state: "Open",
    to_state: "InProgress",
    actor: "bob@example.com",
  },
];

describe("EntityTimeline", () => {
  it("renders all event actions", () => {
    render(<EntityTimeline events={mockEvents} />);
    expect(screen.getByText("create")).toBeInTheDocument();
    expect(screen.getByText("start_work")).toBeInTheDocument();
  });

  it("renders from and to states", () => {
    render(<EntityTimeline events={mockEvents} />);
    expect(screen.getByText("(init)")).toBeInTheDocument();
    expect(screen.getByText("InProgress")).toBeInTheDocument();
  });

  it("shows actor information", () => {
    render(<EntityTimeline events={mockEvents} />);
    expect(screen.getByText(/by system/)).toBeInTheDocument();
    expect(screen.getByText(/bob@example\.com/)).toBeInTheDocument();
  });

  it("shows empty state when no events", () => {
    render(<EntityTimeline events={[]} />);
    expect(screen.getByText("No events recorded yet.")).toBeInTheDocument();
  });
});
