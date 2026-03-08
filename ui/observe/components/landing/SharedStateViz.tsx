"use client";

import { useEffect, useState } from "react";

const agents = [
  { label: "PARENT", style: { top: "-10px", left: "50%", transform: "translateX(-50%)" } },
  { label: "CHILD", style: { top: "50%", right: "-10px", transform: "translateY(-50%)" } },
  { label: "EXEC", style: { bottom: "-10px", left: "50%", transform: "translateX(-50%)" } },
  { label: "CHILD", style: { top: "50%", left: "-10px", transform: "translateY(-50%)" } },
];

export default function SharedStateViz() {
  const [activeIdx, setActiveIdx] = useState(0);

  useEffect(() => {
    const interval = setInterval(() => {
      setActiveIdx((prev) => (prev + 1) % agents.length);
    }, 1500);
    return () => clearInterval(interval);
  }, []);

  return (
    <div className="relative w-full h-full flex items-center justify-center">
      <div className="relative w-[240px] h-[240px]">
        {/* Central state layer */}
        <div className="absolute left-1/2 top-1/2 w-[160px] h-8 bg-[rgba(45,212,191,0.12)] border border-teal-400/30 rounded-[3px] -translate-x-1/2 -translate-y-1/2 flex items-center justify-center font-mono text-[9px] text-teal-400 font-bold z-[5] whitespace-nowrap">
          SHARED STATE
        </div>

        {/* Orbit circle */}
        <div className="absolute left-1/2 top-1/2 w-[200px] h-[200px] border border-dashed border-white/[0.04] rounded-full -translate-x-1/2 -translate-y-1/2" />

        {/* Agent nodes */}
        {agents.map((agent, i) => (
          <div
            key={i}
            className={`absolute w-12 h-12 bg-white/[0.02] border rounded-full flex items-center justify-center font-mono text-[8px] font-bold transition-all duration-300 ${
              i === activeIdx
                ? "border-teal-400/30 text-white shadow-[0_0_16px_rgba(45,212,191,0.08)]"
                : "border-white/[0.04] text-zinc-400"
            }`}
            style={agent.style as React.CSSProperties}
          >
            {agent.label}
          </div>
        ))}
      </div>
    </div>
  );
}
