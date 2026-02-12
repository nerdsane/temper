import { describe, it, expect } from "vitest";
import { render, screen, fireEvent, within } from "@testing-library/react";
import CascadeResults from "@/components/CascadeResults";
import type { VerificationLevel } from "@/lib/types";

const passedLevels: VerificationLevel[] = [
  { level: "L0: SMT", passed: true, summary: "All guards satisfiable", duration_ms: 12 },
  { level: "L1: Model Check", passed: true, summary: "47 states explored", duration_ms: 340 },
];

const failedLevels: VerificationLevel[] = [
  { level: "L0: SMT", passed: true, summary: "All guards satisfiable", duration_ms: 8 },
  {
    level: "L2: DST",
    passed: false,
    summary: "Invariant violation at tick 312",
    duration_ms: 980,
    details: "Seed: 42, Tick: 312",
  },
  { level: "L3: PropTest", passed: false, summary: "Skipped (L2 failed)", duration_ms: 0 },
];

describe("CascadeResults", () => {
  it("shows 'All Levels Passed' when allPassed is true", () => {
    render(<CascadeResults levels={passedLevels} allPassed={true} />);
    expect(screen.getByText("All Levels Passed")).toBeInTheDocument();
  });

  it("shows 'Verification Failed' when allPassed is false", () => {
    render(<CascadeResults levels={failedLevels} allPassed={false} />);
    expect(screen.getByText("Verification Failed")).toBeInTheDocument();
  });

  it("shows the correct pass count", () => {
    render(<CascadeResults levels={failedLevels} allPassed={false} />);
    expect(screen.getByText(/1 of 3 levels passed/)).toBeInTheDocument();
  });

  it("renders all level names", () => {
    render(<CascadeResults levels={failedLevels} allPassed={false} />);
    expect(screen.getByText("L0: SMT")).toBeInTheDocument();
    expect(screen.getByText("L2: DST")).toBeInTheDocument();
    expect(screen.getByText("L3: PropTest")).toBeInTheDocument();
  });

  it("shows duration for non-zero levels", () => {
    render(<CascadeResults levels={failedLevels} allPassed={false} />);
    expect(screen.getByText("8ms")).toBeInTheDocument();
    expect(screen.getByText("980ms")).toBeInTheDocument();
  });

  it("expands details on click", () => {
    render(<CascadeResults levels={failedLevels} allPassed={false} />);
    expect(screen.queryByText("Seed: 42, Tick: 312")).not.toBeInTheDocument();
    // Find the button containing "L2: DST" text
    const l2Text = screen.getByText("L2: DST");
    const l2Button = l2Text.closest("button")!;
    fireEvent.click(l2Button);
    expect(screen.getByText("Seed: 42, Tick: 312")).toBeInTheDocument();
  });
});
