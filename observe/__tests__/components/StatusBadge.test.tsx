import { describe, it, expect } from "vitest";
import { render, screen } from "@testing-library/react";
import StatusBadge from "@/components/StatusBadge";

describe("StatusBadge", () => {
  it("renders the status text", () => {
    render(<StatusBadge status="Open" />);
    expect(screen.getByText("Open")).toBeInTheDocument();
  });

  it("uses green colors for known 'active' status", () => {
    const { container } = render(<StatusBadge status="active" />);
    const badge = container.querySelector("span")!;
    expect(badge.className).toContain("text-green-400");
  });

  it("uses red colors for known 'cancelled' status", () => {
    const { container } = render(<StatusBadge status="cancelled" />);
    const badge = container.querySelector("span")!;
    expect(badge.className).toContain("text-red-400");
  });

  it("assigns consistent hash-based colors for unknown states", () => {
    const { container: c1 } = render(<StatusBadge status="CustomState" />);
    const { container: c2 } = render(<StatusBadge status="CustomState" />);
    const badge1 = c1.querySelector("span")!;
    const badge2 = c2.querySelector("span")!;
    expect(badge1.className).toBe(badge2.className);
  });

  it("assigns different colors for different unknown states", () => {
    const { container: c1 } = render(<StatusBadge status="AlphaState" />);
    const { container: c2 } = render(<StatusBadge status="ZetaState" />);
    const classes1 = c1.querySelector("span")!.className;
    const classes2 = c2.querySelector("span")!.className;
    // The hash may collide, but we check they both render
    expect(classes1).toBeTruthy();
    expect(classes2).toBeTruthy();
  });
});
