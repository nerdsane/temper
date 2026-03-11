"use client";

import { useEffect, useRef, useState } from "react";

/**
 * "Watch an Agent Grow" — a step-by-step story told in 5 beats:
 *
 *   Beat 0: Agent alone (brief)
 *   Beat 1: Failures appear one by one
 *   Beat 2: "Pattern detected" — failures dim
 *   Beat 3: Knowledge spec materializes, states progress, verified
 *   Beat 4: Deployed — agent gains capability ring (hold)
 */

const BEAT_MS = 2200;
const BEATS = 5;
const CYCLE_MS = BEAT_MS * BEATS;

export default function GrowthViz() {
  const rafRef = useRef<number>(0);
  const startRef = useRef<number>(0);
  const [beat, setBeat] = useState(0);
  const [beatT, setBeatT] = useState(0); // 0..1 within beat

  useEffect(() => {
    const tick = (now: number) => {
      if (!startRef.current) startRef.current = now;
      const elapsed = (now - startRef.current) % CYCLE_MS;
      setBeat(Math.floor(elapsed / BEAT_MS));
      setBeatT((elapsed % BEAT_MS) / BEAT_MS);
      rafRef.current = requestAnimationFrame(tick);
    };
    rafRef.current = requestAnimationFrame(tick);
    return () => cancelAnimationFrame(rafRef.current);
  }, []);

  const ease = (t: number) =>
    t < 0.5 ? 2 * t * t : 1 - Math.pow(-2 * t + 2, 2) / 2;

  const cl = (v: number) => Math.max(0, Math.min(1, v));

  // Agent position
  const ax = 160, ay = 46;

  // Failure dots
  const failures = [
    { x: 90, y: 130, label: "bug #41" },
    { x: 160, y: 120, label: "retry" },
    { x: 230, y: 130, label: "lost ctx" },
    { x: 160, y: 155, label: "re-investigate" },
  ];

  // Spec states
  const specStates = ["Draft", "Indexed", "Linked"];

  // Visibility logic per beat
  const failuresVisible = beat >= 1;
  const failureCount = beat === 1 ? Math.min(4, Math.floor(beatT * 5)) : beat >= 2 ? 4 : 0;
  const failuresDim = beat >= 2;
  const patternVisible = beat >= 2;
  const specVisible = beat >= 3;
  const specStateIdx = beat === 3 ? Math.min(2, Math.floor(beatT * 3.5)) : beat >= 4 ? 2 : 0;
  const verified = beat === 3 && beatT > 0.75 || beat >= 4;
  const deployed = beat >= 4;
  const capRing = beat >= 4;

  return (
    <svg className="w-full h-full" viewBox="0 0 320 320" fill="none">
      <defs>
        <filter id="gv-glow" x="-100%" y="-100%" width="300%" height="300%">
          <feGaussianBlur stdDeviation="5" result="b" />
          <feMerge><feMergeNode in="b" /><feMergeNode in="SourceGraphic" /></feMerge>
        </filter>
        <filter id="gv-soft" x="-50%" y="-50%" width="200%" height="200%">
          <feGaussianBlur stdDeviation="3" result="b" />
          <feMerge><feMergeNode in="b" /><feMergeNode in="SourceGraphic" /></feMerge>
        </filter>
      </defs>

      {/* ── Agent node ── */}
      <circle cx={ax} cy={ay} r="18"
        style={{ fill: 'var(--color-bg-elevated)', stroke: 'var(--color-border)' }} strokeWidth="1.5" />
      <text x={ax} y={ay + 1} textAnchor="middle" dominantBaseline="central"
        style={{ fill: 'var(--color-text-secondary)' }} fontSize="11" fontFamily="var(--font-mono), monospace">
        {"◈"}
      </text>
      <text x={ax} y={ay - 28} textAnchor="middle"
        style={{ fill: 'var(--color-text-muted)' }} fontSize="7"
        fontFamily="var(--font-mono), monospace" fontWeight="700" letterSpacing="0.1em">
        AGENT
      </text>

      {/* Capability ring (beat 4) */}
      {capRing && (
        <circle cx={ax} cy={ay} r="24" fill="none"
          style={{ stroke: 'var(--color-accent-teal)' }} strokeWidth="1" strokeDasharray="4 3"
          opacity={ease(beatT) * 0.5} filter="url(#gv-soft)" />
      )}

      {/* ── Dashed line from agent down ── */}
      <line x1={ax} y1={ay + 18} x2={ax} y2={100}
        style={{ stroke: 'var(--color-border)' }} strokeWidth="1" strokeDasharray="3 3" />

      {/* ── Beat 1-2: Failure attempts ── */}
      {failures.map((f, i) => {
        const show = failuresVisible && i < failureCount;
        const dim = failuresDim;
        const o = show ? (dim ? 0.25 : 1) : 0;

        return (
          <g key={`f-${i}`} opacity={o}>
            <circle cx={f.x} cy={f.y} r="7"
              style={{ fill: 'var(--color-accent-pink-dim)', stroke: 'color-mix(in srgb, var(--color-accent-pink) 25%, transparent)' }} strokeWidth="1" />
            {/* X mark */}
            <line x1={f.x - 3} y1={f.y - 3} x2={f.x + 3} y2={f.y + 3}
              style={{ stroke: 'var(--color-accent-pink)' }} strokeWidth="1.5" strokeLinecap="round" />
            <line x1={f.x + 3} y1={f.y - 3} x2={f.x - 3} y2={f.y + 3}
              style={{ stroke: 'var(--color-accent-pink)' }} strokeWidth="1.5" strokeLinecap="round" />
            <text x={f.x} y={f.y + 16} textAnchor="middle"
              style={{ fill: 'var(--color-accent-pink-glow)' }} fontSize="6"
              fontFamily="var(--font-mono), monospace">
              {f.label}
            </text>
          </g>
        );
      })}

      {/* ── Beat 2: Pattern detected ── */}
      {patternVisible && (
        <g opacity={beat === 2 ? ease(cl(beatT / 0.4)) : beat >= 3 ? 0.3 : 0}>
          <rect x={105} y={185} width={110} height={18} rx="3"
            style={{ fill: 'var(--color-bg-elevated)', stroke: 'var(--color-border)' }} strokeWidth="0.5" />
          <text x={160} y={195} textAnchor="middle" dominantBaseline="central"
            style={{ fill: 'var(--color-text-secondary)' }} fontSize="7"
            fontFamily="var(--font-mono), monospace" letterSpacing="0.04em">
            {"▸ pattern detected"}
          </text>
        </g>
      )}

      {/* ── Beat 3-4: Knowledge spec box ── */}
      {specVisible && (
        <g opacity={beat === 3 ? ease(cl(beatT / 0.2)) : 1}>
          {/* Spec container */}
          <rect x={80} y={215} width={160} height={55} rx="4"
            style={{
              fill: 'var(--color-bg-elevated)',
              stroke: verified ? 'color-mix(in srgb, var(--color-accent-teal) 35%, transparent)' : 'var(--color-border)',
            }}
            strokeWidth="1" />

          {/* Verified outer glow */}
          {verified && (
            <rect x={80} y={215} width={160} height={55} rx="4"
              fill="none" style={{ stroke: 'var(--color-accent-teal)' }} strokeWidth="1.5"
              opacity={0.15} filter="url(#gv-glow)" />
          )}

          {/* Spec title */}
          <text x={100} y={229}
            style={{ fill: verified ? 'var(--color-accent-teal)' : 'var(--color-text-muted)' }}
            fontSize="7.5" fontFamily="var(--font-mono), monospace"
            fontWeight="700" letterSpacing="0.08em">
            KNOWLEDGE
          </text>

          {/* Verified badge */}
          {verified && (
            <g>
              <circle cx={222} cy={227} r="5"
                style={{ fill: 'color-mix(in srgb, var(--color-accent-teal) 12%, transparent)', stroke: 'var(--color-accent-teal)' }} strokeWidth="0.5" />
              <text x={222} y={228} textAnchor="middle" dominantBaseline="central"
                style={{ fill: 'var(--color-accent-teal)' }} fontSize="7">✓</text>
            </g>
          )}

          {/* State pills */}
          {specStates.map((label, i) => {
            const active = i === specStateIdx;
            const past = i < specStateIdx;
            return (
              <g key={`ss-${i}`}>
                <rect x={90 + i * 48} y={240} width={40} height={16} rx="2"
                  style={{
                    fill: active ? 'var(--color-accent-teal-dim)' : past ? 'color-mix(in srgb, var(--color-accent-teal) 4%, transparent)' : 'var(--color-bg-elevated)',
                    stroke: active ? 'var(--color-accent-teal-glow)' : past ? 'color-mix(in srgb, var(--color-accent-teal) 15%, transparent)' : 'var(--color-border)',
                  }}
                  strokeWidth="0.5" />
                <text x={110 + i * 48} y={249} textAnchor="middle" dominantBaseline="central"
                  style={{
                    fill: active ? 'var(--color-accent-teal)' : past ? 'color-mix(in srgb, var(--color-accent-teal) 50%, transparent)' : 'var(--color-text-muted)',
                  }}
                  fontSize="6.5" fontFamily="var(--font-mono), monospace">
                  {label}
                </text>
                {i < specStates.length - 1 && (
                  <text x={134 + i * 48} y={249} textAnchor="middle" dominantBaseline="central"
                    style={{ fill: past ? 'color-mix(in srgb, var(--color-accent-teal) 30%, transparent)' : 'var(--color-border)' }}
                    fontSize="7" fontFamily="var(--font-mono), monospace">→</text>
                )}
              </g>
            );
          })}
        </g>
      )}

      {/* ── Beat 4: Deployed ── */}
      {deployed && (
        <g opacity={ease(cl(beatT / 0.3))}>
          <rect x={120} y={282} width={80} height={18} rx="3"
            style={{ fill: 'color-mix(in srgb, var(--color-accent-teal) 6%, transparent)', stroke: 'var(--color-accent-teal-glow-mid)' }} strokeWidth="0.5" />
          <text x={160} y={292} textAnchor="middle" dominantBaseline="central"
            style={{ fill: 'var(--color-accent-teal)' }} fontSize="7"
            fontFamily="var(--font-mono), monospace" fontWeight="700"
            letterSpacing="0.06em" filter="url(#gv-soft)">
            ◈ DEPLOYED
          </text>
        </g>
      )}

      {/* ── Beat label (subtle, bottom-right) ── */}
      <text x={300} y={310} textAnchor="end"
        style={{ fill: 'var(--color-border)' }} fontSize="6"
        fontFamily="var(--font-mono), monospace">
        {beat === 0 ? "" : beat === 1 ? "failures accumulate" : beat === 2 ? "pattern detected" : beat === 3 ? "spec generated" : "capability deployed"}
      </text>
    </svg>
  );
}
