"use client";

import { useState } from "react";
import type { WorkflowStep } from "@/lib/types";

const STEP_LABELS: Record<string, string> = {
  loaded: "Spec Loaded",
  verify_started: "Verification Started",
  L0_symbolic: "L0 Symbolic (SMT)",
  L1_model_check: "L1 Model Check",
  L2_simulation: "L2 Simulation (DST)",
  L3_property_test: "L3 Property Test",
  deployed: "Deployed",
};

function StepIcon({ status, passed }: { status: string; passed?: boolean }) {
  if (status === "completed" && passed === false) {
    return (
      <div className="w-7 h-7 rounded-full bg-red-950 border-2 border-red-500 flex items-center justify-center">
        <svg className="w-3.5 h-3.5 text-red-400" fill="none" stroke="currentColor" viewBox="0 0 24 24">
          <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M6 18L18 6M6 6l12 12" />
        </svg>
      </div>
    );
  }
  if (status === "completed") {
    return (
      <div className="w-7 h-7 rounded-full bg-green-950 border-2 border-green-500 flex items-center justify-center">
        <svg className="w-3.5 h-3.5 text-green-400" fill="none" stroke="currentColor" viewBox="0 0 24 24">
          <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M5 13l4 4L19 7" />
        </svg>
      </div>
    );
  }
  if (status === "running") {
    return (
      <div className="w-7 h-7 rounded-full bg-yellow-950 border-2 border-yellow-500 flex items-center justify-center">
        <div className="w-2.5 h-2.5 bg-yellow-400 rounded-full animate-pulse" />
      </div>
    );
  }
  if (status === "failed") {
    return (
      <div className="w-7 h-7 rounded-full bg-red-950 border-2 border-red-500 flex items-center justify-center">
        <svg className="w-3.5 h-3.5 text-red-400" fill="none" stroke="currentColor" viewBox="0 0 24 24">
          <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M6 18L18 6M6 6l12 12" />
        </svg>
      </div>
    );
  }
  // pending
  return (
    <div className="w-7 h-7 rounded-full bg-gray-900 border-2 border-gray-600 flex items-center justify-center">
      <div className="w-2 h-2 bg-gray-500 rounded-full" />
    </div>
  );
}

interface WorkflowTimelineProps {
  steps: WorkflowStep[];
  entityType?: string;
}

export default function WorkflowTimeline({ steps }: WorkflowTimelineProps) {
  const [expanded, setExpanded] = useState<Set<number>>(new Set());

  const toggle = (index: number) => {
    setExpanded((prev) => {
      const next = new Set(prev);
      if (next.has(index)) next.delete(index);
      else next.add(index);
      return next;
    });
  };

  return (
    <div className="relative">
      {/* Vertical timeline line */}
      <div className="absolute left-[13px] top-4 bottom-4 w-px bg-gray-700" />

      <div className="space-y-0">
        {steps.map((step, i) => {
          const label = STEP_LABELS[step.step] ?? step.step;
          const isExpanded = expanded.has(i);
          const timeStr = step.timestamp
            ? new Date(step.timestamp).toLocaleTimeString("en-US", {
                hour: "2-digit",
                minute: "2-digit",
                second: "2-digit",
                hour12: false,
              })
            : null;

          return (
            <div key={step.step} className="relative flex gap-3 py-2">
              {/* Step dot */}
              <div className="relative z-10 flex-shrink-0">
                <StepIcon status={step.status} passed={step.passed} />
              </div>

              {/* Step content */}
              <button
                onClick={() => step.summary && toggle(i)}
                className={`flex-1 text-left rounded-lg p-2.5 min-w-0 transition-colors ${
                  step.summary ? "hover:bg-gray-800/50 cursor-pointer" : "cursor-default"
                }`}
              >
                <div className="flex items-center justify-between">
                  <span className={`text-sm font-medium ${
                    step.status === "completed" && step.passed !== false
                      ? "text-gray-200"
                      : step.status === "running"
                      ? "text-yellow-300"
                      : step.status === "failed" || step.passed === false
                      ? "text-red-300"
                      : "text-gray-500"
                  }`}>
                    {label}
                  </span>
                  <div className="flex items-center gap-2">
                    {timeStr && (
                      <span className="text-xs font-mono text-gray-500">{timeStr}</span>
                    )}
                    {step.passed !== undefined && step.status === "completed" && (
                      <span className={`text-xs font-mono px-1.5 py-0.5 rounded ${
                        step.passed
                          ? "bg-green-900/50 text-green-400"
                          : "bg-red-900/50 text-red-400"
                      }`}>
                        {step.passed ? "PASS" : "FAIL"}
                      </span>
                    )}
                  </div>
                </div>

                {/* Summary (always visible for completed steps) */}
                {step.summary && (isExpanded || step.status === "running") && (
                  <div className="text-xs text-gray-400 mt-1 font-mono">
                    {step.summary}
                  </div>
                )}
              </button>
            </div>
          );
        })}
      </div>
    </div>
  );
}
