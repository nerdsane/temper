"use client";

import { useEffect, useState, useRef } from "react";

/**
 * "What's a Tempered Agent?" visualization.
 *
 * Shows a central agent core with capability rings that accumulate
 * over time, demonstrating how a tempered agent gains capabilities:
 *   Core → Tasks ring → Knowledge ring → Notifications ring → (reset)
 */

const rings = [
  { label: "TASKS", radius: 52 },
  { label: "KNOWLEDGE", radius: 82 },
  { label: "NOTIFICATIONS", radius: 112 },
];

const CYCLE_MS = 6000;
const RING_APPEAR_MS = 1200;
const HOLD_MS = CYCLE_MS - rings.length * RING_APPEAR_MS;

export default function TemperedAgentViz() {
  const [ringOpacities, setRingOpacities] = useState<number[]>([0, 0, 0]);
  const [glowIdx, setGlowIdx] = useState(-1);
  const rafRef = useRef<number>(0);
  const startRef = useRef<number>(0);

  useEffect(() => {
    const tick = (now: number) => {
      if (!startRef.current) startRef.current = now;
      const elapsed = (now - startRef.current) % CYCLE_MS;

      const newOpacities = [0, 0, 0];
      let activeGlow = -1;

      for (let i = 0; i < rings.length; i++) {
        const ringStart = i * RING_APPEAR_MS;
        const ringEnd = ringStart + RING_APPEAR_MS;

        if (elapsed >= ringStart && elapsed < ringEnd) {
          const t = (elapsed - ringStart) / RING_APPEAR_MS;
          const eased = t < 0.5 ? 2 * t * t : 1 - Math.pow(-2 * t + 2, 2) / 2;
          newOpacities[i] = eased;
          activeGlow = i;
        } else if (elapsed >= ringEnd) {
          const holdEnd = rings.length * RING_APPEAR_MS + HOLD_MS;
          if (elapsed < holdEnd) {
            newOpacities[i] = 1;
          } else {
            const fadeT = (elapsed - holdEnd) / (CYCLE_MS - holdEnd);
            const fadeEased = 1 - fadeT * fadeT;
            newOpacities[i] = Math.max(0, fadeEased);
          }
        }
      }

      setRingOpacities(newOpacities);
      setGlowIdx(activeGlow);
      rafRef.current = requestAnimationFrame(tick);
    };

    rafRef.current = requestAnimationFrame(tick);
    return () => cancelAnimationFrame(rafRef.current);
  }, []);

  const cx = 160;
  const cy = 160;

  return (
    <svg className="w-full h-full" viewBox="0 0 320 320" fill="none">
      <defs>
        <filter id="ta-core-glow" x="-100%" y="-100%" width="300%" height="300%">
          <feGaussianBlur stdDeviation="6" result="b" />
          <feMerge>
            <feMergeNode in="b" />
            <feMergeNode in="SourceGraphic" />
          </feMerge>
        </filter>
        <filter id="ta-ring-glow" x="-100%" y="-100%" width="300%" height="300%">
          <feGaussianBlur stdDeviation="10" result="b" />
          <feMerge>
            <feMergeNode in="b" />
            <feMergeNode in="SourceGraphic" />
          </feMerge>
        </filter>
        <filter id="ta-soft" x="-50%" y="-50%" width="200%" height="200%">
          <feGaussianBlur stdDeviation="3" result="b" />
          <feMerge>
            <feMergeNode in="b" />
            <feMergeNode in="SourceGraphic" />
          </feMerge>
        </filter>
      </defs>

      {/* Capability rings */}
      {rings.map((ring, i) => {
        const opacity = ringOpacities[i];
        if (opacity <= 0) return null;

        const isGlowing = i === glowIdx;

        return (
          <g key={ring.label} opacity={opacity}>
            {/* Glow ring (only during appearance) */}
            {isGlowing && (
              <circle
                cx={cx}
                cy={cy}
                r={ring.radius}
                fill="none"
                style={{ stroke: 'var(--color-accent-teal)' }}
                strokeWidth="2"
                opacity={0.35 * opacity}
                filter="url(#ta-ring-glow)"
              />
            )}

            {/* Ring background fill */}
            <circle
              cx={cx}
              cy={cy}
              r={ring.radius}
              fill="none"
              style={{
                stroke: isGlowing
                  ? 'color-mix(in srgb, var(--color-accent-teal) 25%, transparent)'
                  : 'color-mix(in srgb, var(--color-accent-teal) 8%, transparent)'
              }}
              strokeWidth="1.5"
              strokeDasharray="4 3"
            />

            {/* Label at top of ring */}
            <text
              x={cx}
              y={cy - ring.radius - 6}
              textAnchor="middle"
              style={{
                fill: isGlowing
                  ? 'var(--color-accent-teal)'
                  : 'color-mix(in srgb, var(--color-accent-teal) 45%, transparent)'
              }}
              fontSize="7"
              fontFamily="var(--font-mono), monospace"
              fontWeight="700"
              letterSpacing="0.1em"
            >
              {ring.label}
            </text>

            {/* Subtle marker dots at cardinal points */}
            {[0, 90, 180, 270].map((angle) => {
              const rad = (angle * Math.PI) / 180;
              const dx = cx + ring.radius * Math.cos(rad);
              const dy = cy + ring.radius * Math.sin(rad);
              return (
                <circle
                  key={angle}
                  cx={dx}
                  cy={dy}
                  r="1.5"
                  style={{
                    fill: isGlowing
                      ? 'color-mix(in srgb, var(--color-accent-teal) 50%, transparent)'
                      : 'var(--color-border)'
                  }}
                />
              );
            })}
          </g>
        );
      })}

      {/* Core agent node — always visible */}
      <circle
        cx={cx}
        cy={cy}
        r="22"
        fill="none"
        style={{ stroke: 'var(--color-accent-teal)' }}
        strokeWidth="1"
        opacity="0.15"
        filter="url(#ta-soft)"
      />
      <circle
        cx={cx}
        cy={cy}
        r="16"
        style={{
          fill: 'color-mix(in srgb, var(--color-accent-teal) 8%, transparent)',
          stroke: 'color-mix(in srgb, var(--color-accent-teal) 45%, transparent)'
        }}
        strokeWidth="1.5"
      />
      <circle
        cx={cx}
        cy={cy}
        r="5"
        style={{ fill: 'var(--color-accent-teal)' }}
        opacity="0.7"
        filter="url(#ta-core-glow)"
      />
      <circle cx={cx} cy={cy} r="2" style={{ fill: 'var(--color-text-primary)' }} />

      {/* Core label */}
      <text
        x={cx}
        y={cy + 30}
        textAnchor="middle"
        style={{ fill: 'var(--color-text-muted)' }}
        fontSize="7"
        fontFamily="var(--font-mono), monospace"
        fontWeight="600"
        letterSpacing="0.08em"
      >
        AGENT
      </text>
    </svg>
  );
}
