"use client";

import { useState } from "react";
import type { WorkflowStep, VerificationDetail } from "@/lib/types";
import { VerificationDetailsPanel } from "@/components/CascadeResults";

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
      <div className="w-6 h-6 rounded-full bg-[var(--color-accent-pink-dim)] flex items-center justify-center transition-all duration-300">
        <svg className="w-3 h-3 text-[var(--color-accent-pink)]" fill="none" stroke="currentColor" viewBox="0 0 24 24">
          <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M6 18L18 6M6 6l12 12" />
        </svg>
      </div>
    );
  }
  if (status === "completed") {
    return (
      <div className="w-6 h-6 rounded-full bg-[var(--color-accent-teal-dim)] flex items-center justify-center transition-all duration-300">
        <svg className="w-3 h-3 text-[var(--color-accent-teal)]" fill="none" stroke="currentColor" viewBox="0 0 24 24">
          <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M5 13l4 4L19 7" />
        </svg>
      </div>
    );
  }
  if (status === "running") {
    return (
      <div className="w-6 h-6 rounded-full bg-[var(--color-accent-pink-dim)] flex items-center justify-center transition-all duration-300">
        <div className="w-2 h-2 bg-[var(--color-accent-pink)] rounded-full animate-pulse" />
      </div>
    );
  }
  if (status === "failed") {
    return (
      <div className="w-6 h-6 rounded-full bg-[var(--color-accent-pink-dim)] flex items-center justify-center transition-all duration-300">
        <svg className="w-3 h-3 text-[var(--color-accent-pink)]" fill="none" stroke="currentColor" viewBox="0 0 24 24">
          <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M6 18L18 6M6 6l12 12" />
        </svg>
      </div>
    );
  }
  // pending
  return (
    <div className="w-6 h-6 rounded-full bg-[var(--color-bg-elevated)] flex items-center justify-center transition-all duration-300">
      <div className="w-1.5 h-1.5 bg-[var(--color-text-muted)] rounded-full" />
    </div>
  );
}

/** Map from step name (e.g. "L2_simulation") to verification details for that level. */
export type StepDetailsMap = Record<string, VerificationDetail[]>;

interface WorkflowTimelineProps {
  steps: WorkflowStep[];
  entityType?: string;
  /** Verification details keyed by step name, for rendering rich failure info. */
  stepDetails?: StepDetailsMap;
}

export default function WorkflowTimeline({ steps, stepDetails }: WorkflowTimelineProps) {
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
      <div className="absolute left-[11px] top-3 bottom-3 w-px bg-gradient-to-b from-[var(--color-border)] via-[var(--color-bg-elevated)] to-transparent" />

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
                className={`flex-1 text-left rounded-[2px] p-2 min-w-0 transition-colors ${
                  step.summary ? "hover:bg-[var(--color-bg-elevated)] cursor-pointer" : "cursor-default"
                }`}
              >
                <div className="flex items-center justify-between">
                  <span className={`text-[13px] font-medium ${
                    step.status === "completed" && step.passed !== false
                      ? "text-[var(--color-text-secondary)]"
                      : step.status === "running"
                      ? "text-[var(--color-accent-pink)]"
                      : step.status === "failed" || step.passed === false
                      ? "text-[var(--color-accent-pink)]"
                      : "text-[var(--color-text-muted)]"
                  }`}>
                    {label}
                  </span>
                  <div className="flex items-center gap-2">
                    {timeStr && (
                      <span className="text-[11px] font-mono text-[var(--color-text-muted)]">{timeStr}</span>
                    )}
                    {(step.passed === true || step.passed === false) && step.status === "completed" && (
                      <span className={`text-[10px] font-mono px-1.5 py-0.5 rounded-full ${
                        step.passed
                          ? "bg-[var(--color-accent-teal-dim)] text-[var(--color-accent-teal)] animate-flash-teal"
                          : "bg-[var(--color-accent-pink-dim)] text-[var(--color-accent-pink)] animate-flash-pink"
                      }`}>
                        {step.passed ? "PASS" : "FAIL"}
                      </span>
                    )}
                  </div>
                </div>

                {/* Summary — auto-visible for failed steps */}
                {step.summary && (isExpanded || step.status === "running" || step.passed === false) && (
                  <div className="text-[11px] text-[var(--color-text-secondary)] mt-1 font-mono">
                    {step.summary}
                  </div>
                )}
              </button>

              {/* Verification detail panel for failed steps */}
              {isExpanded && step.passed === false && stepDetails?.[step.step] && (
                <div className="ml-8 mt-1 mb-2">
                  <VerificationDetailsPanel details={stepDetails[step.step]} />
                </div>
              )}
            </div>
          );
        })}
      </div>
    </div>
  );
}
