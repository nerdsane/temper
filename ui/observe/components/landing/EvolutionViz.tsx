"use client";

import { useEffect, useState } from "react";

const labels = ["TRAJECTORY", "PATTERN", "PROPOSAL", "HOT-SWAP"];

const positions: Record<string, string>[] = [
  { top: "10px", left: "50%", transform: "translateX(-50%)" },
  { top: "50%", right: "0", transform: "translateY(-50%)" },
  { bottom: "10px", left: "50%", transform: "translateX(-50%)" },
  { top: "50%", left: "0", transform: "translateY(-50%)" },
];

export default function EvolutionViz() {
  const [activeIdx, setActiveIdx] = useState(0);

  useEffect(() => {
    const interval = setInterval(() => {
      setActiveIdx((prev) => (prev + 1) % labels.length);
    }, 1000);
    return () => clearInterval(interval);
  }, []);

  return (
    <div className="w-[220px] h-[220px] relative m-5">
      <svg className="w-full h-full overflow-visible" viewBox="0 0 300 300">
        {/* Background dashed circle */}
        <circle
          cx="150"
          cy="150"
          r="100"
          fill="none"
          stroke="rgba(255,255,255,0.04)"
          strokeWidth="2"
          strokeDasharray="6 6"
        />
        {/* Animated main path */}
        <path
          d="M150 50 A100 100 0 1 1 149.9 50"
          fill="none"
          stroke="#2dd4bf"
          strokeWidth="2"
          strokeLinecap="round"
          strokeDasharray="600"
          strokeDashoffset="600"
          className="animate-dash-loop"
        />
      </svg>

      {/* Labels */}
      {labels.map((label, i) => (
        <div
          key={i}
          className={`absolute font-mono text-[9px] font-bold transition-colors duration-300 ${
            i === activeIdx ? "text-teal-400" : "text-zinc-600"
          }`}
          style={positions[i]}
        >
          {label}
        </div>
      ))}
    </div>
  );
}
