"use client";

import { useEffect, useState } from "react";

const blocks = [
  "automaton: Memory",
  "state: archived",
  "action: commit",
  "invariant: valid",
  "policy: bot_1",
  "wasm: store",
];

export default function SpecViz() {
  const [activeIdx, setActiveIdx] = useState(0);

  useEffect(() => {
    const interval = setInterval(() => {
      setActiveIdx((prev) => (prev + 1) % blocks.length);
    }, 1200);
    return () => clearInterval(interval);
  }, []);

  return (
    <div className="grid grid-cols-2 gap-2 w-full p-6">
      {blocks.map((text, i) => (
        <div
          key={i}
          className={`bg-white/[0.02] border rounded-[3px] p-3.5 font-mono text-[11px] transition-all duration-500 [transition-timing-function:cubic-bezier(0.16,1,0.3,1)] ${
            i === activeIdx
              ? "border-teal-400/30 text-white bg-[rgba(45,212,191,0.12)] -translate-y-0.5 shadow-[inset_0_1px_0_rgba(255,255,255,0.04),0_4px_24px_rgba(45,212,191,0.04)]"
              : "border-white/[0.04] text-zinc-600"
          }`}
        >
          {text}
        </div>
      ))}
    </div>
  );
}
