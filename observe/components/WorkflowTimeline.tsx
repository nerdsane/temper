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
      <div className="w-6 h-6 rounded-full bg-pink-400/10 flex items-center justify-center transition-all duration-300">
        <svg className="w-3 h-3 text-pink-400" fill="none" stroke="currentColor" viewBox="0 0 24 24">
          <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M6 18L18 6M6 6l12 12" />
        </svg>
      </div>
    );
  }
  if (status === "completed") {
    return (
      <div className="w-6 h-6 rounded-full bg-teal-400/10 flex items-center justify-center transition-all duration-300">
        <svg className="w-3 h-3 text-teal-400" fill="none" stroke="currentColor" viewBox="0 0 24 24">
          <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M5 13l4 4L19 7" />
        </svg>
      </div>
    );
  }
  if (status === "running") {
    return (
      <div className="w-6 h-6 rounded-full bg-pink-400/10 flex items-center justify-center transition-all duration-300">
        <div className="w-2 h-2 bg-pink-400 rounded-full animate-pulse" />
      </div>
    );
  }
  if (status === "failed") {
    return (
      <div className="w-6 h-6 rounded-full bg-pink-400/10 flex items-center justify-center transition-all duration-300">
        <svg className="w-3 h-3 text-pink-400" fill="none" stroke="currentColor" viewBox="0 0 24 24">
          <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M6 18L18 6M6 6l12 12" />
        </svg>
      </div>
    );
  }
  // pending
  return (
    <div className="w-6 h-6 rounded-full bg-white/[0.02] flex items-center justify-center transition-all duration-300">
      <div className="w-1.5 h-1.5 bg-zinc-700 rounded-full" />
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
      <div className="absolute left-[11px] top-3 bottom-3 w-px bg-gradient-to-b from-white/[0.06] via-white/[0.04] to-transparent" />

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
            <div key={step.step} className="relative flex gap-2.5 py-1.5">
              {/* Step dot */}
              <div className="relative z-10 flex-shrink-0">
                <StepIcon status={step.status} passed={step.passed} />
              </div>

              {/* Step content */}
              <button
                onClick={() => step.summary && toggle(i)}
                className={`flex-1 text-left rounded-lg p-2 min-w-0 transition-colors ${
                  step.summary ? "hover:bg-white/[0.02] cursor-pointer" : "cursor-default"
                }`}
              >
                <div className="flex items-center justify-between">
                  <span className={`text-[13px] font-medium ${
                    step.status === "completed" && step.passed !== false
                      ? "text-zinc-300"
                      : step.status === "running"
                      ? "text-pink-400"
                      : step.status === "failed" || step.passed === false
                      ? "text-pink-400"
                      : "text-zinc-600"
                  }`}>
                    {label}
                  </span>
                  <div className="flex items-center gap-2">
                    {timeStr && (
                      <span className="text-[11px] font-mono text-zinc-600">{timeStr}</span>
                    )}
                    {(step.passed === true || step.passed === false) && step.status === "completed" && (
                      <span className={`text-[10px] font-mono px-1.5 py-0.5 rounded-full ${
                        step.passed
                          ? "bg-teal-400/10 text-teal-400 animate-flash-teal"
                          : "bg-pink-400/10 text-pink-400 animate-flash-pink"
                      }`}>
                        {step.passed ? "PASS" : "FAIL"}
                      </span>
                    )}
                  </div>
                </div>

                {/* Summary — auto-visible for failed steps */}
                {step.summary && (isExpanded || step.status === "running" || step.passed === false) && (
                  <div className="text-[11px] text-zinc-500 mt-1 font-mono">
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
