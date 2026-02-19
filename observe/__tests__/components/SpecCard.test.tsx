import { describe, it, expect } from "vitest";
import { render, screen } from "@testing-library/react";
import { vi } from "vitest";
import SpecCard from "@/components/SpecCard";
import type { SpecSummary } from "@/lib/types";

vi.mock("next/link", () => ({
  default: ({ children, href, ...props }: { children: React.ReactNode; href: string; [key: string]: unknown }) => (
    <a href={href} {...props}>{children}</a>
  ),
}));

const mockPush = vi.fn();
vi.mock("next/navigation", () => ({
  useRouter: () => ({ push: mockPush, replace: vi.fn(), back: vi.fn(), forward: vi.fn(), prefetch: vi.fn(), refresh: vi.fn() }),
}));

const mockSpec: SpecSummary = {
  entity_type: "Ticket",
  states: ["Open", "InProgress", "Closed"],
  actions: ["assign", "close"],
  initial_state: "Open",
};

describe("SpecCard", () => {
  it("renders the entity type name", () => {
    render(<SpecCard spec={mockSpec} />);
    expect(screen.getByText("Ticket")).toBeInTheDocument();
  });

  it("displays the state count", () => {
    render(<SpecCard spec={mockSpec} />);
    expect(screen.getByText("3")).toBeInTheDocument();
  });

  it("displays the action count", () => {
    render(<SpecCard spec={mockSpec} />);
    expect(screen.getByText("2")).toBeInTheDocument();
  });

  it("highlights the initial state with lime styling", () => {
    render(<SpecCard spec={mockSpec} />);
    // "Open" appears multiple times - once as initial state value, once as state badge
    const openElements = screen.getAllByText("Open");
    const limeElement = openElements.find(
      (el) => el.className.includes("text-lime-400"),
    );
    expect(limeElement).toBeTruthy();
  });

  it("renders all state badges", () => {
    render(<SpecCard spec={mockSpec} />);
    expect(screen.getByText("InProgress")).toBeInTheDocument();
    expect(screen.getByText("Closed")).toBeInTheDocument();
  });

  it("links to the spec detail page", () => {
    render(<SpecCard spec={mockSpec} />);
    const specLink = screen.getAllByRole("link").find(
      (a) => a.getAttribute("href") === "/specs/Ticket",
    );
    expect(specLink).toBeTruthy();
  });

  it("has a verify button", () => {
    render(<SpecCard spec={mockSpec} />);
    const verifyBtn = screen.getByText("Verify");
    expect(verifyBtn.tagName).toBe("BUTTON");
  });
});
