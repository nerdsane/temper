import { describe, it, expect } from "vitest";
import { render } from "@testing-library/react";
import StateMachineGraph from "@/components/StateMachineGraph";
import type { SpecDetail } from "@/lib/types";

const mockSpec: SpecDetail = {
  entity_type: "Ticket",
  states: ["Open", "InProgress", "Closed"],
  initial_state: "Open",
  actions: [
    { name: "start_work", kind: "input", from: ["Open"], to: "InProgress", guards: [], effects: [] },
    { name: "close", kind: "input", from: ["InProgress"], to: "Closed", guards: [], effects: [] },
  ],
  invariants: [
    { name: "no_further_transitions", when: ["Closed"], assertion: "no outgoing transitions" },
  ],
  state_variables: [],
};

describe("StateMachineGraph", () => {
  it("renders an SVG element", () => {
    const { container } = render(<StateMachineGraph spec={mockSpec} />);
    const svg = container.querySelector("svg");
    expect(svg).toBeTruthy();
  });

  it("renders correct number of state nodes", () => {
    const { container } = render(<StateMachineGraph spec={mockSpec} />);
    const texts = container.querySelectorAll("text");
    const stateTexts = Array.from(texts).filter((t) =>
      mockSpec.states.includes(t.textContent || ""),
    );
    expect(stateTexts.length).toBe(3);
  });

  it("renders edge labels", () => {
    const { container } = render(<StateMachineGraph spec={mockSpec} />);
    const texts = Array.from(container.querySelectorAll("text"));
    const edgeLabels = texts.filter((t) => t.textContent === "start_work" || t.textContent === "close");
    expect(edgeLabels.length).toBe(2);
  });

  it("renders initial state indicator", () => {
    const { container } = render(<StateMachineGraph spec={mockSpec} />);
    const circles = container.querySelectorAll("circle");
    expect(circles.length).toBeGreaterThan(0);
  });

  it("renders terminal state with dashed border", () => {
    const { container } = render(<StateMachineGraph spec={mockSpec} />);
    const dashedRects = Array.from(container.querySelectorAll("rect")).filter(
      (r) => r.getAttribute("stroke-dasharray") !== null,
    );
    expect(dashedRects.length).toBe(1);
  });
});
