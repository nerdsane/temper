"use client";

import { useState, useCallback } from "react";
import type { PendingDecision, PolicyScopeMatrix } from "@/lib/types";
import type { PolicyBuilderContext } from "./PolicyBuilder";
import PolicyBuilder from "./PolicyBuilder";
import { commonContext } from "@/lib/decision-grouping";

interface BatchApproveBarProps {
  selectedDecisions: PendingDecision[];
  onApprove: (ids: string[], matrix: PolicyScopeMatrix) => Promise<{ succeeded: number; failed: number }>;
  onClear: () => void;
}

export default function BatchApproveBar({
  selectedDecisions,
  onApprove,
  onClear,
}: BatchApproveBarProps) {
  const [showBuilder, setShowBuilder] = useState(false);
  const [approving, setApproving] = useState(false);
  const [result, setResult] = useState<{ succeeded: number; failed: number } | null>(null);

  const ctx: PolicyBuilderContext = commonContext(selectedDecisions);
  const ids = selectedDecisions.map((d) => d.id);

  const handleApprove = useCallback(
    async (matrix: PolicyScopeMatrix) => {
      setApproving(true);
      setResult(null);
      try {
        const res = await onApprove(ids, matrix);
        setResult(res);
        if (res.failed === 0) {
          setTimeout(() => {
            setShowBuilder(false);
            setResult(null);
          }, 1500);
        }
      } finally {
        setApproving(false);
      }
    },
    [ids, onApprove],
  );

  if (selectedDecisions.length === 0) return null;

  return (
    <div className="fixed bottom-0 left-0 right-0 z-50 border-t border-[var(--color-border)] bg-[color-mix(in_srgb,var(--color-bg-primary)_95%,transparent)] backdrop-blur-md">
      <div className="max-w-5xl mx-auto px-6 py-3">
        {/* Summary row */}
        <div className="flex items-center justify-between">
          <div className="flex items-center gap-3">
            <div className="w-2 h-2 rounded-full bg-[var(--color-accent-teal)] animate-pulse" />
            <span className="text-sm text-[var(--color-text-primary)]">
              <span className="font-mono font-semibold">{selectedDecisions.length}</span>{" "}
              {selectedDecisions.length === 1 ? "decision" : "decisions"} selected
            </span>
          </div>
          <div className="flex items-center gap-2">
            {result && (
              <span className={`text-xs font-mono ${result.failed > 0 ? "text-[var(--color-accent-pink)]" : "text-[var(--color-accent-teal)]"}`}>
                {result.succeeded} approved{result.failed > 0 ? `, ${result.failed} failed` : ""}
              </span>
            )}
            <button
              type="button"
              onClick={() => setShowBuilder(!showBuilder)}
              className="px-3 py-1.5 text-xs bg-[var(--color-accent-teal-dim)] text-[var(--color-accent-teal)] rounded hover:bg-[var(--color-accent-teal-dim)] transition-colors"
            >
              {showBuilder ? "Hide" : "Approve Selected"}
            </button>
            <button
              type="button"
              onClick={onClear}
              className="px-3 py-1.5 text-xs bg-[var(--color-bg-elevated)] text-[var(--color-text-secondary)] rounded hover:bg-[var(--color-border)] transition-colors"
            >
              Clear
            </button>
          </div>
        </div>

        {/* Expanded PolicyBuilder */}
        {showBuilder && (
          <div className="mt-3 pt-3 border-t border-[var(--color-border)]">
            <PolicyBuilder
              context={ctx}
              onApprove={handleApprove}
              onCancel={() => setShowBuilder(false)}
              disabled={approving}
            />
          </div>
        )}
      </div>
    </div>
  );
}
