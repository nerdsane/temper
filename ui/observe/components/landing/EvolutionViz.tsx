"use client";

import { useEffect, useState, useRef } from "react";

/**
 * Evolution feedback loop visualization.
 *
 * Shows four stages arranged in a rounded path with a glowing particle
 * that travels continuously between them:
 *   TRAJECTORY → PATTERN → PROPOSAL → HOT-SWAP → (repeat)
 */

const stages = [
  { label: "TRAJECTORY", sublabel: "Record", icon: "◈" },
  { label: "PATTERN", sublabel: "Detect", icon: "◇" },
  { label: "PROPOSAL", sublabel: "Generate", icon: "△" },
  { label: "HOT-SWAP", sublabel: "Deploy", icon: "⬡" },
];

// Node positions (centered in 340x320 viewBox, shifted right for left label room)
const nodes = [
  { x: 170, y: 60 },   // top
  { x: 260, y: 160 },  // right
  { x: 170, y: 260 },  // bottom
  { x: 80, y: 160 },   // left
];

// Label positions relative to nodes (inside SVG)
const labelAnchors: { dx: number; dy: number; anchor: "start" | "middle" | "end" }[] = [
  { dx: 0, dy: -28, anchor: "middle" },     // top → above
  { dx: 28, dy: 0, anchor: "start" },       // right → right of node
  { dx: 0, dy: 32, anchor: "middle" },      // bottom → below
  { dx: -28, dy: 0, anchor: "end" },        // left → left of node
];

// Quadratic bezier control points for each segment (matching rounded rect corners)
const controlPoints = [
  { x: 260, y: 60 },   // top → right
  { x: 260, y: 260 },  // right → bottom
  { x: 80, y: 260 },   // bottom → left
  { x: 80, y: 60 },    // left → top
];

export default function EvolutionViz() {
  const [activeIdx, setActiveIdx] = useState(0);
  const [progress, setProgress] = useState(0);
  const rafRef = useRef<number>(0);
  const startRef = useRef<number>(0);

  const CYCLE_MS = 3600;
  const STAGE_MS = CYCLE_MS / stages.length;

  useEffect(() => {
    const tick = (now: number) => {
      if (!startRef.current) startRef.current = now;
      const elapsed = (now - startRef.current) % CYCLE_MS;
      const stageIdx = Math.floor(elapsed / STAGE_MS);
      const stageProgress = (elapsed % STAGE_MS) / STAGE_MS;

      setActiveIdx(stageIdx);
      setProgress(stageProgress);
      rafRef.current = requestAnimationFrame(tick);
    };

    rafRef.current = requestAnimationFrame(tick);
    return () => cancelAnimationFrame(rafRef.current);
  }, []);

  // Quadratic bezier interpolation for particle position
  const getParticlePos = (idx: number, t: number) => {
    const from = nodes[idx];
    const to = nodes[(idx + 1) % nodes.length];
    const cp = controlPoints[idx];
    const u = 1 - t;
    return {
      x: u * u * from.x + 2 * u * t * cp.x + t * t * to.x,
      y: u * u * from.y + 2 * u * t * cp.y + t * t * to.y,
    };
  };

  const particle = getParticlePos(activeIdx, progress);

  // Build the full loop path
  const loopPath = `M${nodes[0].x},${nodes[0].y} Q${controlPoints[0].x},${controlPoints[0].y} ${nodes[1].x},${nodes[1].y} Q${controlPoints[1].x},${controlPoints[1].y} ${nodes[2].x},${nodes[2].y} Q${controlPoints[2].x},${controlPoints[2].y} ${nodes[3].x},${nodes[3].y} Q${controlPoints[3].x},${controlPoints[3].y} ${nodes[0].x},${nodes[0].y}`;

  return (
    <svg
      className="w-full h-full"
      viewBox="-30 0 380 320"
      fill="none"
    >
      <defs>
        <filter id="evo-glow" x="-100%" y="-100%" width="300%" height="300%">
          <feGaussianBlur stdDeviation="8" result="blur" />
          <feMerge>
            <feMergeNode in="blur" />
            <feMergeNode in="SourceGraphic" />
          </feMerge>
        </filter>
        <filter id="node-glow" x="-50%" y="-50%" width="200%" height="200%">
          <feGaussianBlur stdDeviation="4" result="blur" />
          <feMerge>
            <feMergeNode in="blur" />
            <feMergeNode in="SourceGraphic" />
          </feMerge>
        </filter>
      </defs>

      {/* Background loop path (dashed) */}
      <path
        d={loopPath}
        style={{ stroke: 'var(--color-border)' }}
        strokeWidth="1"
        strokeDasharray="6 4"
        fill="none"
      />

      {/* Trail: a partial stroke that follows behind the particle */}
      <path
        d={loopPath}
        style={{ stroke: 'var(--color-accent-teal)' }}
        strokeWidth="1.5"
        strokeLinecap="round"
        fill="none"
        opacity="0.15"
        strokeDasharray="200 800"
        strokeDashoffset={-((activeIdx + progress) / stages.length) * 1000 + 200}
      />

      {/* Direction arrows along the path midpoints */}
      {nodes.map((node, i) => {
        const next = nodes[(i + 1) % nodes.length];
        const cp = controlPoints[i];
        const mx = 0.25 * node.x + 0.5 * cp.x + 0.25 * next.x;
        const my = 0.25 * node.y + 0.5 * cp.y + 0.25 * next.y;
        const simpleAngle = Math.atan2(next.y - node.y, next.x - node.x);

        return (
          <g
            key={`arrow-${i}`}
            transform={`translate(${mx}, ${my}) rotate(${(simpleAngle * 180) / Math.PI})`}
            opacity={i === activeIdx ? 0.6 : 0.12}
            className="transition-opacity duration-500"
          >
            <path
              d="M-3,-3 L2,0 L-3,3"
              fill="none"
              style={{ stroke: 'var(--color-accent-teal)' }}
              strokeWidth="1"
              strokeLinecap="round"
              strokeLinejoin="round"
            />
          </g>
        );
      })}

      {/* Nodes */}
      {nodes.map((node, i) => {
        const isActive = i === activeIdx;

        return (
          <g key={`node-${i}`}>
            {/* Active glow ring */}
            {isActive && (
              <circle
                cx={node.x}
                cy={node.y}
                r="18"
                fill="none"
                style={{ stroke: 'var(--color-accent-teal)' }}
                strokeWidth="1"
                opacity={0.3 * (1 - progress * 0.6)}
                filter="url(#node-glow)"
              />
            )}
            {/* Node circle */}
            <circle
              cx={node.x}
              cy={node.y}
              r="13"
              style={{
                fill: isActive
                  ? 'color-mix(in srgb, var(--color-accent-teal) 12%, transparent)'
                  : 'var(--color-bg-elevated)',
                stroke: isActive
                  ? 'color-mix(in srgb, var(--color-accent-teal) 60%, transparent)'
                  : 'var(--color-border)'
              }}
              strokeWidth="1.5"
            />
            {/* Icon */}
            <text
              x={node.x}
              y={node.y + 1}
              textAnchor="middle"
              dominantBaseline="central"
              style={{
                fill: isActive
                  ? 'var(--color-accent-teal)'
                  : 'var(--color-text-muted)'
              }}
              fontSize="10"
            >
              {stages[i].icon}
            </text>
          </g>
        );
      })}

      {/* Traveling particle */}
      <circle
        cx={particle.x}
        cy={particle.y}
        r="5"
        style={{ fill: 'var(--color-accent-teal)' }}
        opacity="0.8"
        filter="url(#evo-glow)"
      />
      <circle
        cx={particle.x}
        cy={particle.y}
        r="2"
        style={{ fill: 'var(--color-text-primary)' }}
      />

      {/* Labels (inside SVG to avoid overflow clipping) */}
      {stages.map((stage, i) => {
        const isActive = i === activeIdx;
        const la = labelAnchors[i];
        const node = nodes[i];

        return (
          <g key={`label-${i}`}>
            <text
              x={node.x + la.dx}
              y={node.y + la.dy}
              textAnchor={la.anchor}
              dominantBaseline={i === 2 ? "hanging" : i === 0 ? "auto" : "central"}
              style={{
                fill: isActive
                  ? 'var(--color-accent-teal)'
                  : 'var(--color-text-muted)'
              }}
              fontSize="8"
              fontFamily="var(--font-mono), monospace"
              fontWeight="700"
              letterSpacing="0.08em"
            >
              {stage.label}
            </text>
            <text
              x={node.x + la.dx}
              y={node.y + la.dy + (i === 0 ? -11 : i === 2 ? 13 : 12)}
              textAnchor={la.anchor}
              dominantBaseline={i === 2 ? "hanging" : i === 0 ? "auto" : "central"}
              style={{
                fill: isActive
                  ? 'var(--color-text-secondary)'
                  : 'color-mix(in srgb, var(--color-text-muted) 50%, transparent)'
              }}
              fontSize="7"
              fontFamily="var(--font-mono), monospace"
            >
              {stage.sublabel}
            </text>
          </g>
        );
      })}
    </svg>
  );
}
