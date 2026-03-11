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
          className={`bg-[var(--color-bg-elevated)] border rounded-[3px] p-3.5 font-mono text-[11px] transition-all duration-500 [transition-timing-function:cubic-bezier(0.16,1,0.3,1)] ${
            i === activeIdx
              ? "border-[var(--color-accent-teal)]/30 text-[var(--color-text-primary)] bg-[var(--color-accent-teal-dim)] -translate-y-0.5 shadow-[inset_0_1px_0_var(--color-border),0_4px_24px_var(--color-accent-teal-dim)]"
              : "border-[var(--color-border)] text-[var(--color-text-muted)]"
          }`}
        >
          {text}
        </div>
      ))}
    </div>
  );
}
