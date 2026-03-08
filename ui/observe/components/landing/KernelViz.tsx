"use client";

import { useEffect, useRef, useState } from "react";

/**
 * The Kernel visualization — layered architecture diagram.
 *
 * Four stacked layers (bottom to top):
 *   PERSISTENCE  →  CEDAR  →  VERIFICATION  →  SPECS
 *
 * A teal pulse travels upward through the stack, lighting each
 * layer in sequence to represent data flowing through the kernel.
 */

const LAYERS = [
  { id: "persistence", label: "PERSISTENCE", sub: "event sourcing" },
  { id: "cedar", label: "CEDAR", sub: "authorization" },
  { id: "verification", label: "VERIFICATION", sub: "proof engine" },
  { id: "specs", label: "SPECS", sub: "spec interpreter" },
] as const;

const CYCLE_MS = 4000;
const LAYER_COUNT = LAYERS.length;

// Layout
const CX = 160;
const LAYER_W = 200;
const LAYER_H = 38;
const GAP = 10;
const STACK_H = LAYER_COUNT * LAYER_H + (LAYER_COUNT - 1) * GAP;
const BASE_Y = (320 - STACK_H) / 2 + STACK_H; // bottom of stack

export default function KernelViz() {
  const [activeLayer, setActiveLayer] = useState(0);
  const [progress, setProgress] = useState(0);
  const rafRef = useRef<number>(0);
  const startRef = useRef<number>(0);

  useEffect(() => {
    const tick = (now: number) => {
      if (!startRef.current) startRef.current = now;
      const elapsed = (now - startRef.current) % CYCLE_MS;
      const phase = elapsed / CYCLE_MS;

      // Each layer gets 1/LAYER_COUNT of the cycle
      const layerIdx = Math.min(
        Math.floor(phase * LAYER_COUNT),
        LAYER_COUNT - 1
      );
      const layerProgress = (phase * LAYER_COUNT) % 1;

      setActiveLayer(layerIdx);
      setProgress(layerProgress);
      rafRef.current = requestAnimationFrame(tick);
    };
    rafRef.current = requestAnimationFrame(tick);
    return () => cancelAnimationFrame(rafRef.current);
  }, []);

  // Ease in-out quad
  const ease = (t: number) =>
    t < 0.5 ? 2 * t * t : 1 - Math.pow(-2 * t + 2, 2) / 2;

  // Compute layer Y positions (bottom to top, index 0 = bottom)
  const layerY = (idx: number) =>
    BASE_Y - idx * (LAYER_H + GAP) - LAYER_H;

  // Pulse travels from bottom of current layer to top
  const pulseFromY = layerY(activeLayer) + LAYER_H;
  const pulseToY = layerY(activeLayer);
  const pulseT = ease(Math.min(progress * 1.3, 1));
  const pulseY = pulseFromY + (pulseToY - pulseFromY) * pulseT;
  const pulseVisible = progress < 0.75;

  // Connector line: runs full height of stack
  const connectorTop = layerY(LAYER_COUNT - 1) + LAYER_H / 2;
  const connectorBottom = layerY(0) + LAYER_H / 2;

  return (
    <svg className="w-full h-full" viewBox="0 0 320 320" fill="none">
      <defs>
        <filter id="k-glow" x="-100%" y="-100%" width="300%" height="300%">
          <feGaussianBlur stdDeviation="6" result="b" />
          <feMerge>
            <feMergeNode in="b" />
            <feMergeNode in="SourceGraphic" />
          </feMerge>
        </filter>
        <filter id="k-soft" x="-50%" y="-50%" width="200%" height="200%">
          <feGaussianBlur stdDeviation="3" result="b" />
          <feMerge>
            <feMergeNode in="b" />
            <feMergeNode in="SourceGraphic" />
          </feMerge>
        </filter>
      </defs>

      {/* Title */}
      <text
        x={CX}
        y={layerY(LAYER_COUNT - 1) - 20}
        textAnchor="middle"
        fill="rgba(255,255,255,0.12)"
        fontSize="7"
        fontFamily="var(--font-mono), monospace"
        letterSpacing="0.14em"
        fontWeight="500"
      >
        THE KERNEL
      </text>

      {/* Vertical connector line (center spine) */}
      <line
        x1={CX}
        y1={connectorTop}
        x2={CX}
        y2={connectorBottom}
        stroke="rgba(255,255,255,0.04)"
        strokeWidth="1"
        strokeDasharray="3 3"
      />

      {/* Layer rectangles (bottom to top) */}
      {LAYERS.map((layer, idx) => {
        const y = layerY(idx);
        const x = CX - LAYER_W / 2;
        const isActive = idx === activeLayer;
        const isPast = idx < activeLayer;
        // Layers that were already pulsed in this cycle stay subtly lit
        const litOpacity = isPast ? 0.5 : 0;

        return (
          <g key={layer.id}>
            {/* Subtle outer glow when active */}
            {isActive && (
              <rect
                x={x - 2}
                y={y - 2}
                width={LAYER_W + 4}
                height={LAYER_H + 4}
                rx="6"
                fill="none"
                stroke="#2dd4bf"
                strokeWidth="1"
                opacity={0.15}
                filter="url(#k-soft)"
              />
            )}

            {/* Layer body */}
            <rect
              x={x}
              y={y}
              width={LAYER_W}
              height={LAYER_H}
              rx="4"
              fill={
                isActive
                  ? "rgba(45,212,191,0.08)"
                  : isPast
                    ? "rgba(45,212,191,0.03)"
                    : "rgba(255,255,255,0.02)"
              }
              stroke={
                isActive
                  ? "rgba(45,212,191,0.4)"
                  : isPast
                    ? `rgba(45,212,191,${0.08 + litOpacity * 0.1})`
                    : "rgba(255,255,255,0.04)"
              }
              strokeWidth={isActive ? "1.5" : "1"}
            />

            {/* Layer label */}
            <text
              x={CX}
              y={y + LAYER_H / 2 - 4}
              textAnchor="middle"
              fill={
                isActive
                  ? "#2dd4bf"
                  : isPast
                    ? "rgba(45,212,191,0.4)"
                    : "rgba(255,255,255,0.2)"
              }
              fontSize="8"
              fontFamily="var(--font-mono), monospace"
              fontWeight="700"
              letterSpacing="0.08em"
            >
              {layer.label}
            </text>

            {/* Sublabel */}
            <text
              x={CX}
              y={y + LAYER_H / 2 + 8}
              textAnchor="middle"
              fill={
                isActive
                  ? "rgba(45,212,191,0.6)"
                  : isPast
                    ? "rgba(45,212,191,0.2)"
                    : "rgba(255,255,255,0.1)"
              }
              fontSize="7"
              fontFamily="var(--font-mono), monospace"
            >
              {layer.sub}
            </text>

            {/* Arrow indicator between layers (except above top layer) */}
            {idx < LAYER_COUNT - 1 && (
              <text
                x={CX}
                y={y - GAP / 2 + 1}
                textAnchor="middle"
                dominantBaseline="central"
                fill={
                  idx === activeLayer - 1 || idx === activeLayer
                    ? "rgba(45,212,191,0.3)"
                    : "rgba(255,255,255,0.06)"
                }
                fontSize="8"
                fontFamily="var(--font-mono), monospace"
              >
                {"^"}
              </text>
            )}
          </g>
        );
      })}

      {/* Traveling pulse */}
      {pulseVisible && (
        <>
          <circle
            cx={CX}
            cy={pulseY}
            r="4"
            fill="#2dd4bf"
            opacity="0.85"
            filter="url(#k-glow)"
          />
          <circle cx={CX} cy={pulseY} r="1.5" fill="white" />
        </>
      )}
    </svg>
  );
}
