"use client";

import { useState } from "react";
import type { VerificationLevel } from "@/lib/types";

interface CascadeResultsProps {
  levels: VerificationLevel[];
  allPassed: boolean;
}

export default function CascadeResults({ levels, allPassed }: CascadeResultsProps) {
  const [expanded, setExpanded] = useState<Set<number>>(new Set());

  const toggle = (index: number) => {
    setExpanded((prev) => {
      const next = new Set(prev);
      if (next.has(index)) {
        next.delete(index);
      } else {
        next.add(index);
      }
      return next;
    });
  };

  return (
    <div className="space-y-3">
      {/* Overall status */}
      <div
        className={`flex items-center gap-3 p-4 rounded-lg border ${
          allPassed
            ? "bg-green-950/30 border-green-800"
            : "bg-red-950/30 border-red-800"
        }`}
      >
        <span className={`text-2xl ${allPassed ? "text-green-400" : "text-red-400"}`}>
          {allPassed ? "\u2713" : "\u2717"}
        </span>
        <div>
          <div className={`font-semibold ${allPassed ? "text-green-400" : "text-red-400"}`}>
            {allPassed ? "All Levels Passed" : "Verification Failed"}
          </div>
          <div className="text-sm text-gray-400">
            {levels.filter((l) => l.passed).length} of {levels.length} levels passed
          </div>
        </div>
      </div>

      {/* Per-level results */}
      {levels.map((level, i) => {
        const isExpanded = expanded.has(i);
        const isSkipped = level.summary.startsWith("Skipped");

        return (
          <div
            key={i}
            className={`border rounded-lg overflow-hidden ${
              level.passed
                ? "border-gray-800 bg-gray-900"
                : isSkipped
                ? "border-gray-800 bg-gray-900/50"
                : "border-red-900 bg-red-950/20"
            }`}
          >
            <button
              onClick={() => toggle(i)}
              className="w-full flex items-center justify-between p-4 text-left hover:bg-gray-800/50 transition-colors"
            >
              <div className="flex items-center gap-3">
                <span
                  className={`text-lg ${
                    level.passed
                      ? "text-green-400"
                      : isSkipped
                      ? "text-gray-600"
                      : "text-red-400"
                  }`}
                >
                  {level.passed ? "\u2713" : isSkipped ? "\u2014" : "\u2717"}
                </span>
                <div>
                  <div className="font-mono text-sm text-gray-200">{level.level}</div>
                  <div className="text-sm text-gray-400 mt-0.5">{level.summary}</div>
                </div>
              </div>
              <div className="flex items-center gap-3">
                {level.duration_ms !== undefined && level.duration_ms > 0 && (
                  <span className="text-xs font-mono text-gray-500">
                    {level.duration_ms}ms
                  </span>
                )}
                <span className="text-gray-500 text-sm">
                  {isExpanded ? "\u25B2" : "\u25BC"}
                </span>
              </div>
            </button>

            {isExpanded && level.details && (
              <div className="px-4 pb-4 border-t border-gray-800">
                <pre className="mt-3 p-3 bg-gray-950 rounded text-xs font-mono text-gray-300 whitespace-pre-wrap overflow-x-auto">
                  {level.details}
                </pre>
              </div>
            )}
          </div>
        );
      })}
    </div>
  );
}
