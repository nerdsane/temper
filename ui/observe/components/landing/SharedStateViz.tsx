"use client";

import { useEffect, useState, useRef } from "react";

/**
 * Verified Shared State visualization.
 *
 * Clean vertical hierarchy:
 *   Agent A + Agent B (top)
 *        ↕
 *   [ Shared State Layer ]  (center)
 *        ↕
 *     Executor (bottom)
 *
 * Pulses animate read/write operations between agents and entities.
 */

const CYCLE_MS = 4500;
const BEAT_MS = CYCLE_MS / 3;

// Layout constants — strict grid alignment
const CX = 160; // center x
const COL_L = 110; // left column
const COL_R = 210; // right column

const ROW_AGENT = 52;  // agent row y
const ROW_STATE = 160; // state layer center y
const ROW_EXEC = 268;  // executor row y

const agents = [
  { id: "A", label: "AGENT A", x: COL_L, y: ROW_AGENT },
  { id: "B", label: "AGENT B", x: COL_R, y: ROW_AGENT },
  { id: "E", label: "EXECUTOR", x: CX, y: ROW_EXEC },
];

const entities = [
  { label: "Task", x: COL_L, y: ROW_STATE, states: ["Open", "Claimed", "Done"] },
  { label: "Knowledge", x: COL_R, y: ROW_STATE, states: ["Draft", "Indexed", "Linked"] },
];

// 3-beat story
const beats = [
  { agentIdx: 0, entityIdx: 0, stateIdx: 1, dir: "write" as const },
  { agentIdx: 1, entityIdx: 1, stateIdx: 1, dir: "read" as const },
  { agentIdx: 2, entityIdx: 0, stateIdx: 2, dir: "read" as const },
];

export default function SharedStateViz() {
  const [beat, setBeat] = useState(0);
  const [progress, setProgress] = useState(0);
  const rafRef = useRef<number>(0);
  const startRef = useRef<number>(0);

  useEffect(() => {
    const tick = (now: number) => {
      if (!startRef.current) startRef.current = now;
      const elapsed = (now - startRef.current) % CYCLE_MS;
      setBeat(Math.floor(elapsed / BEAT_MS));
      setProgress((elapsed % BEAT_MS) / BEAT_MS);
      rafRef.current = requestAnimationFrame(tick);
    };
    rafRef.current = requestAnimationFrame(tick);
    return () => cancelAnimationFrame(rafRef.current);
  }, []);

  const current = beats[beat];
  const activeAgent = agents[current.agentIdx];
  const activeEntity = entities[current.entityIdx];

  // Ease function
  const ease = (t: number) =>
    t < 0.5 ? 2 * t * t : 1 - Math.pow(-2 * t + 2, 2) / 2;

  // Pulse position
  const pulseT = Math.min(ease(progress * 1.4), 1);
  const fromX = current.dir === "write" ? activeAgent.x : activeEntity.x;
  const fromY = current.dir === "write" ? activeAgent.y : activeEntity.y;
  const toX = current.dir === "write" ? activeEntity.x : activeAgent.x;
  const toY = current.dir === "write" ? activeEntity.y : activeAgent.y;
  const pulseX = fromX + (toX - fromX) * pulseT;
  const pulseY = fromY + (toY - fromY) * pulseT;
  const pulseVisible = progress < 0.7;
  const entityLit = progress > 0.45;

  return (
    <svg className="w-full h-full" viewBox="0 0 320 320" fill="none">
      <defs>
        <filter id="ss-glow" x="-100%" y="-100%" width="300%" height="300%">
          <feGaussianBlur stdDeviation="6" result="b" />
          <feMerge><feMergeNode in="b" /><feMergeNode in="SourceGraphic" /></feMerge>
        </filter>
        <filter id="ss-soft" x="-50%" y="-50%" width="200%" height="200%">
          <feGaussianBlur stdDeviation="3" result="b" />
          <feMerge><feMergeNode in="b" /><feMergeNode in="SourceGraphic" /></feMerge>
        </filter>
      </defs>

      {/* ── Connection lines (clean verticals + one diagonal) ── */}
      {/* Agent A → Task (straight down) */}
      <line x1={COL_L} y1={ROW_AGENT + 18} x2={COL_L} y2={ROW_STATE - 22}
        style={{ stroke: beat === 0 ? 'var(--color-accent-teal-glow-mid)' : 'var(--color-border)' }}
        strokeWidth="1" />
      {/* Agent B → Order (straight down) */}
      <line x1={COL_R} y1={ROW_AGENT + 18} x2={COL_R} y2={ROW_STATE - 22}
        style={{ stroke: beat === 1 ? 'var(--color-accent-teal-glow-mid)' : 'var(--color-border)' }}
        strokeWidth="1" />
      {/* Executor → Task (angled up-left) */}
      <line x1={CX} y1={ROW_EXEC - 18} x2={COL_L} y2={ROW_STATE + 22}
        style={{ stroke: beat === 2 ? 'var(--color-accent-teal-glow-mid)' : 'var(--color-border)' }}
        strokeWidth="1" />
      {/* Executor → Order (angled up-right, dormant) */}
      <line x1={CX} y1={ROW_EXEC - 18} x2={COL_R} y2={ROW_STATE + 22}
        style={{ stroke: 'var(--color-border)' }} strokeWidth="1" strokeDasharray="4 4" />

      {/* ── State layer container ── */}
      <rect
        x="72" y={ROW_STATE - 30} width="176" height="60" rx="4"
        style={{ fill: 'var(--color-bg-elevated)', stroke: 'var(--color-border)' }}
        strokeWidth="1"
      />
      <text x={CX} y={ROW_STATE - 35}
        textAnchor="middle" style={{ fill: 'var(--color-text-muted)' }}
        fontSize="6.5" fontFamily="var(--font-mono), monospace"
        letterSpacing="0.12em" fontWeight="500">
        EVENT-SOURCED STATE
      </text>

      {/* ── Entity boxes ── */}
      {entities.map((ent, ei) => {
        const lit = ei === current.entityIdx && entityLit;
        return (
          <g key={ent.label}>
            <rect
              x={ent.x - 28} y={ent.y - 16} width="56" height="32" rx="3"
              style={{
                fill: lit ? 'var(--color-accent-teal-dim)' : 'var(--color-bg-elevated)',
                stroke: lit ? 'color-mix(in srgb, var(--color-accent-teal) 35%, transparent)' : 'var(--color-border)',
              }}
              strokeWidth="1"
            />
            <text x={ent.x} y={ent.y - 4} textAnchor="middle"
              style={{ fill: lit ? 'var(--color-accent-teal)' : 'var(--color-text-muted)' }}
              fontSize="8" fontFamily="var(--font-mono), monospace" fontWeight="700">
              {ent.label}
            </text>
            <text x={ent.x} y={ent.y + 9} textAnchor="middle"
              style={{ fill: lit ? 'color-mix(in srgb, var(--color-accent-teal) 60%, transparent)' : 'var(--color-text-muted)' }}
              fontSize="7" fontFamily="var(--font-mono), monospace">
              {lit ? ent.states[current.stateIdx] : ent.states[0]}
            </text>
          </g>
        );
      })}

      {/* ── Agent nodes ── */}
      {agents.map((a) => {
        const active = a.id === activeAgent.id;
        const labelY = a.id === "E" ? a.y + 26 : a.y - 24;
        const actionY = a.id === "E" ? a.y + 36 : a.y - 34;

        return (
          <g key={a.id}>
            {active && (
              <circle cx={a.x} cy={a.y} r="19" fill="none"
                style={{ stroke: 'var(--color-accent-teal)' }} strokeWidth="1" opacity={0.2} filter="url(#ss-soft)" />
            )}
            <circle cx={a.x} cy={a.y} r="14"
              style={{
                fill: active ? 'var(--color-accent-teal-dim)' : 'var(--color-bg-elevated)',
                stroke: active ? 'color-mix(in srgb, var(--color-accent-teal) 45%, transparent)' : 'var(--color-border)',
              }}
              strokeWidth="1.5" />
            <text x={a.x} y={a.y + 1} textAnchor="middle" dominantBaseline="central"
              style={{ fill: active ? 'var(--color-accent-teal)' : 'var(--color-text-muted)' }}
              fontSize="9" fontFamily="var(--font-mono), monospace" fontWeight="700">
              {a.id}
            </text>
            <text x={a.x} y={labelY} textAnchor="middle"
              style={{ fill: active ? 'color-mix(in srgb, var(--color-accent-teal) 70%, transparent)' : 'var(--color-text-muted)' }}
              fontSize="7" fontFamily="var(--font-mono), monospace"
              fontWeight="600" letterSpacing="0.06em">
              {a.label}
            </text>
            {active && progress > 0.4 && (
              <text x={a.x} y={actionY} textAnchor="middle"
                style={{ fill: 'var(--color-text-secondary)' }} fontSize="6.5"
                fontFamily="var(--font-mono), monospace">
                {current.dir === "write" ? "write ↓" : "read ↑"}
              </text>
            )}
          </g>
        );
      })}

      {/* ── Traveling pulse ── */}
      {pulseVisible && (
        <>
          <circle cx={pulseX} cy={pulseY} r="4"
            style={{ fill: current.dir === "write" ? 'var(--color-accent-teal)' : 'var(--color-accent-violet)' }}
            opacity="0.85" filter="url(#ss-glow)" />
          <circle cx={pulseX} cy={pulseY} r="1.5" style={{ fill: 'var(--color-text-primary)' }} />
        </>
      )}
    </svg>
  );
}
