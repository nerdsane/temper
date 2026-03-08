import { describe, it, expect } from "vitest";
import { render, screen, fireEvent } from "@testing-library/react";
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

  it("renders SMT guard satisfiability table when expanded", () => {
    const levels: VerificationLevel[] = [
      {
        level: "L0: SMT",
        passed: true,
        summary: "All guards satisfiable",
        smt: {
          guard_satisfiability: [["assign", true], ["close", false]],
          inductive_invariants: [["no_orphans", true]],
          unreachable_states: ["Archived"],
          all_passed: false,
        },
      },
    ];
    render(<CascadeResults levels={levels} allPassed={false} />);
    fireEvent.click(screen.getByText("L0: SMT").closest("button")!);
    expect(screen.getByText("Guard Satisfiability")).toBeInTheDocument();
    expect(screen.getByText("assign")).toBeInTheDocument();
    expect(screen.getByText("close")).toBeInTheDocument();
    expect(screen.getByText("Inductive Invariants")).toBeInTheDocument();
    expect(screen.getByText("no_orphans")).toBeInTheDocument();
    expect(screen.getByText("Unreachable States")).toBeInTheDocument();
    expect(screen.getByText("Archived")).toBeInTheDocument();
  });

  it("renders simulation liveness violations when expanded", () => {
    const levels: VerificationLevel[] = [
      {
        level: "L2: DST",
        passed: false,
        summary: "14 liveness violations",
        simulation: {
          all_invariants_held: false,
          ticks: 1000,
          total_transitions: 500,
          total_messages: 200,
          total_dropped: 3,
          violations: [],
          liveness_violations: [
            {
              actor_id: "issue-1",
              property: "eventually_resolves",
              description: "Actor stuck in Open state",
              final_state: { status: "Open", counters: { retries: 5 }, booleans: { assigned: true } },
            },
          ],
          seed: 42,
        },
      },
    ];
    render(<CascadeResults levels={levels} allPassed={false} />);
    fireEvent.click(screen.getByText("L2: DST").closest("button")!);
    expect(screen.getByText("Liveness Violations (1)")).toBeInTheDocument();
    expect(screen.getByText("eventually_resolves")).toBeInTheDocument();
    expect(screen.getByText("Actor stuck in Open state")).toBeInTheDocument();
    expect(screen.getByText("issue-1")).toBeInTheDocument();
    expect(screen.getByText("retries")).toBeInTheDocument();
  });

  it("renders model check counterexamples when expanded", () => {
    const levels: VerificationLevel[] = [
      {
        level: "L1: Model Check",
        passed: false,
        summary: "Property violation found",
        verification: {
          states_explored: 128,
          all_properties_hold: false,
          counterexamples: [
            {
              property: "no_deadlock",
              trace: [
                { status: "Open", counters: { count: 0 }, booleans: {} },
                { status: "Stuck", counters: { count: 1 }, booleans: {} },
              ],
            },
          ],
          is_complete: true,
        },
      },
    ];
    render(<CascadeResults levels={levels} allPassed={false} />);
    fireEvent.click(screen.getByText("L1: Model Check").closest("button")!);
    expect(screen.getByText("Counterexamples (1)")).toBeInTheDocument();
    expect(screen.getByText("no_deadlock")).toBeInTheDocument();
    expect(screen.getByText("128")).toBeInTheDocument();
  });

  it("renders prop test failure details when expanded", () => {
    const levels: VerificationLevel[] = [
      {
        level: "L3: PropTest",
        passed: false,
        summary: "1 failure in 100 cases",
        prop_test: {
          total_cases: 100,
          passed: false,
          failure: {
            invariant: "balance_non_negative",
            action_sequence: ["deposit", "withdraw", "withdraw"],
            final_state: '{ balance: -10 }',
          },
        },
      },
    ];
    render(<CascadeResults levels={levels} allPassed={false} />);
    fireEvent.click(screen.getByText("L3: PropTest").closest("button")!);
    expect(screen.getByText("balance_non_negative")).toBeInTheDocument();
    expect(screen.getByText("Minimal Action Sequence")).toBeInTheDocument();
    expect(screen.getByText("deposit")).toBeInTheDocument();
    expect(screen.getByText("{ balance: -10 }")).toBeInTheDocument();
  });

  it("shows 'No details available' when level has no structured or plain details", () => {
    const levels: VerificationLevel[] = [
      { level: "L0: SMT", passed: true, summary: "All guards satisfiable" },
    ];
    render(<CascadeResults levels={levels} allPassed={true} />);
    fireEvent.click(screen.getByText("L0: SMT").closest("button")!);
    expect(screen.getByText("No details available")).toBeInTheDocument();
  });
});
