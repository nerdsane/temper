"use client";

import { useEffect, useState } from "react";

interface LogEntry {
  agent: string;
  action: string;
  transition: string;
  allowed: boolean;
}

const entries: LogEntry[] = [
  { agent: "agent-a", action: "Claim", transition: "Open \u2192 Claimed", allowed: true },
  { agent: "agent-b", action: "Query", transition: "\u2014", allowed: true },
  { agent: "executor", action: "Start", transition: "Claimed \u2192 Active", allowed: true },
  { agent: "agent-c", action: "Delete", transition: "Active \u2192 ?", allowed: false },
  { agent: "agent-a", action: "Complete", transition: "Active \u2192 Done", allowed: true },
  { agent: "agent-b", action: "Archive", transition: "Done \u2192 Archived", allowed: true },
];

export default function RecordedViz() {
  const [visibleCount, setVisibleCount] = useState(1);
  const [flashIdx, setFlashIdx] = useState(0);

  useEffect(() => {
    const interval = setInterval(() => {
      setVisibleCount((prev) => {
        const next = prev < entries.length ? prev + 1 : 1;
        setFlashIdx(next - 1);
        return next;
      });
    }, 2000);
    return () => clearInterval(interval);
  }, []);

  // Clear flash highlight after animation completes
  useEffect(() => {
    const timeout = setTimeout(() => setFlashIdx(-1), 800);
    return () => clearTimeout(timeout);
  }, [flashIdx]);

  const visibleEntries = entries.slice(0, visibleCount);

  return (
    <div
      className="w-full h-full flex flex-col justify-start overflow-hidden p-4 pt-3 font-mono"
    >
      {/* Column header */}
      <div
        className="text-[7px] uppercase tracking-[0.15em] mb-3 pb-2 flex"
        style={{
          color: "var(--color-text-muted)",
          borderBottom: "1px solid var(--color-border)",
        }}
      >
        <span className="w-[60px] shrink-0">agent</span>
        <span className="w-[52px] shrink-0">action</span>
        <span className="flex-1 min-w-0">transition</span>
        <span className="w-[52px] shrink-0 text-right">auth</span>
      </div>

      {/* Log entries */}
      <div className="flex flex-col gap-[3px]">
        {visibleEntries.map((entry, i) => {
          const isNewest = i === visibleCount - 1;
          const isFlashing = i === flashIdx;
          const age = visibleCount - 1 - i;
          // Newest is full brightness, older entries fade
          const opacity = Math.max(0.3, 1 - age * 0.15);

          return (
            <div
              key={`${visibleCount}-${i}`}
              className={`flex items-center text-[8px] leading-[18px] rounded-[2px] px-2 ${
                isNewest ? "animate-recorded-slide-in" : ""
              } ${isFlashing ? "animate-recorded-flash" : ""}`}
              style={{
                opacity,
                backgroundColor: isFlashing ? undefined : "var(--color-bg-elevated)",
              }}
            >
              <span
                className="w-[60px] shrink-0 truncate"
                style={{ color: "var(--color-text-secondary)" }}
              >
                {entry.agent}
              </span>
              <span
                className="w-[52px] shrink-0"
                style={{
                  color: entry.allowed
                    ? "var(--color-text-primary)"
                    : "var(--color-accent-pink)",
                }}
              >
                {entry.action}
              </span>
              <span
                className="flex-1 min-w-0 truncate"
                style={{ color: "var(--color-text-muted)" }}
              >
                {entry.transition}
              </span>
              <span
                className="w-[52px] shrink-0 text-right font-semibold"
                style={{
                  color: entry.allowed ? "var(--color-accent-teal)" : "var(--color-accent-pink)",
                  fontSize: entry.allowed ? "8px" : "7px",
                }}
              >
                {entry.allowed ? "\u2713" : "\u2717 DENIED"}
              </span>
            </div>
          );
        })}
      </div>
    </div>
  );
}
