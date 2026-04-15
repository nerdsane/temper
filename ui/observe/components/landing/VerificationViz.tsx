"use client";

import { useEffect, useState } from "react";

const layers = [
  "L0 — Z3 SMT Solving",
  "L1 — Model Checking",
  "L2 — DST Simulation",
  "L3 — Property Testing",
];

export default function VerificationViz() {
  const [activeIdx, setActiveIdx] = useState(0);

  useEffect(() => {
    const interval = setInterval(() => {
      setActiveIdx((prev) => (prev + 1) % layers.length);
    }, 2000);
    return () => clearInterval(interval);
  }, []);

  return (
    <div className="flex flex-col gap-2 w-[85%] max-w-[320px]">
      {layers.map((text, i) => (
        <div
          key={i}
          className={`h-12 border rounded-[3px] relative overflow-hidden flex items-center px-4 text-xs font-semibold transition-all duration-400 ${
            i === activeIdx
              ? "border-[var(--color-accent-teal)]/30 text-[var(--color-accent-teal)] bg-[var(--color-accent-teal-dim)] shadow-[inset_0_1px_0_rgba(255,255,255,0.04)]"
              : "bg-[var(--color-bg-elevated)] border-[var(--color-border)] text-[var(--color-text-muted)]"
          }`}
        >
          {text}
          {/* Progress bar */}
          <div
            className={`absolute bottom-0 left-0 h-0.5 bg-[var(--color-accent-teal)] ${
              i === activeIdx ? "animate-filter-progress" : "w-0"
            }`}
          />
        </div>
      ))}
    </div>
  );
}
