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
              ? "border-teal-400/30 text-teal-400 bg-[rgba(45,212,191,0.12)] shadow-[inset_0_1px_0_rgba(255,255,255,0.04)]"
              : "bg-white/[0.02] border-white/[0.04] text-zinc-600"
          }`}
        >
          {text}
          {/* Progress bar */}
          <div
            className={`absolute bottom-0 left-0 h-0.5 bg-teal-400 ${
              i === activeIdx ? "animate-filter-progress" : "w-0"
            }`}
          />
        </div>
      ))}
    </div>
  );
}
