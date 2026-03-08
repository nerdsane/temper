"use client";

import { useEffect, useRef, useState } from "react";

/**
 * "Apps, Not Code" visualization.
 *
 * Shows a small IOA TOML spec snippet at the top styled as a tiny code editor,
 * with a downward flow arrow leading to a 2x2 grid of output badges that
 * light up sequentially: API, PERSISTENCE, AUTH, TELEMETRY.
 */

const specLines = [
  { num: 1, tokens: [{ text: "[automaton]", type: "heading" as const }] },
  {
    num: 2,
    tokens: [
      { text: 'name = ', type: "key" as const },
      { text: '"Task"', type: "value" as const },
    ],
  },
  { num: 3, tokens: [] },
  { num: 4, tokens: [{ text: "[[state]]", type: "heading" as const }] },
  {
    num: 5,
    tokens: [
      { text: 'name = ', type: "key" as const },
      { text: '"Open"', type: "value" as const },
    ],
  },
  { num: 6, tokens: [] },
  { num: 7, tokens: [{ text: "[[state]]", type: "heading" as const }] },
  {
    num: 8,
    tokens: [
      { text: 'name = ', type: "key" as const },
      { text: '"Claimed"', type: "value" as const },
    ],
  },
  { num: 9, tokens: [] },
  { num: 10, tokens: [{ text: "[[transition]]", type: "heading" as const }] },
  {
    num: 11,
    tokens: [
      { text: 'name = ', type: "key" as const },
      { text: '"Claim"', type: "value" as const },
    ],
  },
  {
    num: 12,
    tokens: [
      { text: 'from = ', type: "key" as const },
      { text: '"Open"', type: "value" as const },
    ],
  },
  {
    num: 13,
    tokens: [
      { text: 'to = ', type: "key" as const },
      { text: '"Claimed"', type: "value" as const },
    ],
  },
];

const outputs = [
  { label: "API", sublabel: "OData endpoints" },
  { label: "PERSISTENCE", sublabel: "Event sourcing" },
  { label: "AUTH", sublabel: "Cedar policies" },
  { label: "TELEMETRY", sublabel: "Observability" },
];

const TOKEN_COLORS: Record<string, string> = {
  heading: "#2dd4bf",
  key: "rgba(161,161,170,0.7)",
  value: "#2dd4bf",
};

const CYCLE_MS = 4800;
const STAGE_MS = CYCLE_MS / outputs.length;

export default function AppsViz() {
  const [activeIdx, setActiveIdx] = useState(0);
  const [pulse, setPulse] = useState(0);
  const rafRef = useRef<number>(0);
  const startRef = useRef<number>(0);

  useEffect(() => {
    const tick = (now: number) => {
      if (!startRef.current) startRef.current = now;
      const elapsed = (now - startRef.current) % CYCLE_MS;
      const stageIdx = Math.floor(elapsed / STAGE_MS);
      const stageProgress = (elapsed % STAGE_MS) / STAGE_MS;

      setActiveIdx(stageIdx);
      setPulse(stageProgress);
      rafRef.current = requestAnimationFrame(tick);
    };

    rafRef.current = requestAnimationFrame(tick);
    return () => cancelAnimationFrame(rafRef.current);
  }, []);

  return (
    <div className="flex flex-col items-center w-full h-full p-5 gap-0 justify-center">
      {/* Spec snippet — styled as a tiny code editor */}
      <div
        className="w-full max-w-[280px] rounded-[3px] border border-white/[0.06] bg-white/[0.02] overflow-hidden"
        style={{ boxShadow: "inset 0 1px 0 rgba(255,255,255,0.03)" }}
      >
        {/* Title bar */}
        <div className="flex items-center gap-1.5 px-2.5 py-1.5 border-b border-white/[0.04]">
          <div className="w-[5px] h-[5px] rounded-full bg-white/[0.08]" />
          <div className="w-[5px] h-[5px] rounded-full bg-white/[0.08]" />
          <div className="w-[5px] h-[5px] rounded-full bg-white/[0.08]" />
          <span
            className="ml-1.5 text-zinc-600"
            style={{
              fontFamily: "var(--font-mono), monospace",
              fontSize: "6px",
              letterSpacing: "0.04em",
            }}
          >
            task.ioa.toml
          </span>
        </div>

        {/* Code lines */}
        <div className="py-1.5 px-0">
          {specLines.map((line) => (
            <div
              key={line.num}
              className="flex items-baseline"
              style={{
                fontFamily: "var(--font-mono), monospace",
                fontSize: "7px",
                lineHeight: "13px",
              }}
            >
              {/* Line number */}
              <span
                className="w-[22px] text-right pr-[6px] select-none flex-shrink-0"
                style={{ color: "rgba(113,113,122,0.4)" }}
              >
                {line.num}
              </span>
              {/* Content */}
              <span className="pl-[6px] border-l border-white/[0.04]">
                {line.tokens.length === 0 ? (
                  <span>&nbsp;</span>
                ) : (
                  line.tokens.map((token, j) => (
                    <span
                      key={j}
                      style={{
                        color: TOKEN_COLORS[token.type],
                      }}
                    >
                      {token.text}
                    </span>
                  ))
                )}
              </span>
            </div>
          ))}
        </div>
      </div>

      {/* Flow arrow: spec -> outputs */}
      <svg
        className="my-1 flex-shrink-0"
        width="24"
        height="28"
        viewBox="0 0 24 28"
        fill="none"
      >
        {/* Vertical line */}
        <line
          x1="12"
          y1="0"
          x2="12"
          y2="22"
          stroke="rgba(255,255,255,0.08)"
          strokeWidth="1"
          strokeDasharray="3 2"
        />
        {/* Arrowhead */}
        <path
          d="M8,19 L12,25 L16,19"
          fill="none"
          stroke="#2dd4bf"
          strokeWidth="1"
          strokeLinecap="round"
          strokeLinejoin="round"
          opacity={0.4 + 0.4 * Math.sin(pulse * Math.PI * 2)}
        />
        {/* Traveling dot */}
        <circle
          cx="12"
          cy={2 + pulse * 20}
          r="1.5"
          fill="#2dd4bf"
          opacity={0.6}
        />
      </svg>

      {/* Output badges — 2x2 grid */}
      <div className="grid grid-cols-2 gap-2 w-full max-w-[280px]">
        {outputs.map((output, i) => {
          const isActive = i === activeIdx;
          // Fade intensity: brightest when just activated, fades as pulse progresses
          const intensity = isActive ? 1 - pulse * 0.5 : 0;

          return (
            <div
              key={output.label}
              className="rounded-[3px] border px-3 py-2.5 transition-all duration-500"
              style={{
                borderColor: isActive
                  ? `rgba(45,212,191,${0.2 + intensity * 0.2})`
                  : "rgba(255,255,255,0.04)",
                background: isActive
                  ? `rgba(45,212,191,${0.06 + intensity * 0.08})`
                  : "rgba(255,255,255,0.02)",
                boxShadow: isActive
                  ? `inset 0 1px 0 rgba(255,255,255,0.04), 0 4px 24px rgba(45,212,191,${intensity * 0.06})`
                  : "none",
                transitionTimingFunction: "cubic-bezier(0.16, 1, 0.3, 1)",
              }}
            >
              <div
                className="font-semibold tracking-[0.08em] mb-0.5"
                style={{
                  fontFamily: "var(--font-mono), monospace",
                  fontSize: "8px",
                  color: isActive ? "#2dd4bf" : "rgba(255,255,255,0.2)",
                  transition: "color 500ms cubic-bezier(0.16, 1, 0.3, 1)",
                }}
              >
                {output.label}
              </div>
              <div
                style={{
                  fontFamily: "var(--font-mono), monospace",
                  fontSize: "6.5px",
                  color: isActive
                    ? "rgba(161,161,170,0.8)"
                    : "rgba(255,255,255,0.08)",
                  transition: "color 500ms cubic-bezier(0.16, 1, 0.3, 1)",
                }}
              >
                {output.sublabel}
              </div>
            </div>
          );
        })}
      </div>
    </div>
  );
}
