import { describe, it, expect } from "vitest";
import { render, screen } from "@testing-library/react";
import StatCard from "@/components/StatCard";

describe("StatCard", () => {
  it("renders label and value", () => {
    render(<StatCard label="Total" value={42} />);
    expect(screen.getByText("Total")).toBeInTheDocument();
    expect(screen.getByText("42")).toBeInTheDocument();
  });

  it("renders string value", () => {
    render(<StatCard label="Status" value="OK" />);
    expect(screen.getByText("Status")).toBeInTheDocument();
    expect(screen.getByText("OK")).toBeInTheDocument();
  });

  it("applies custom color class", () => {
    render(<StatCard label="Rate" value="95%" color="text-teal-400" />);
    const valueEl = screen.getByText("95%");
    expect(valueEl.className).toContain("text-teal-400");
  });

  it("uses default color when none provided", () => {
    render(<StatCard label="Count" value={0} />);
    const valueEl = screen.getByText("0");
    expect(valueEl.className).toContain("text-zinc-100");
  });
});
