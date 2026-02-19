"use client";

import { useState } from "react";
import type {
  VerificationLevel,
  SmtDetails,
  SmtState,
  ModelCheckDetails,
  SimulationDetails,
  LivenessViolation,
  InvariantViolation,
  PropTestDetails,
} from "@/lib/types";

interface CascadeResultsProps {
  levels: VerificationLevel[];
  allPassed: boolean;
}

/* ── palette ────────────────────────────────────────── */
const G = "text-teal-400";              // pass accent text
const B = "text-pink-400";              // fail accent text
const G_DOT = "bg-teal-400";
const B_DOT = "bg-pink-400";
const G_GRAD = "from-teal-500/8";
const B_GRAD = "from-pink-500/8";

/* ── shared glass ───────────────────────────────────── */
const GLASS = "bg-white/[0.025] rounded-lg";
const INNER = "bg-white/[0.02] rounded-lg";

/* ── tiny helpers ───────────────────────────────────── */
function Dot({ passed }: { passed: boolean }) {
  return <div className={`w-1.5 h-1.5 rounded-full flex-shrink-0 ${passed ? G_DOT : B_DOT}`} />;
}

function Chevron({ open }: { open: boolean }) {
  return (
    <svg
      className={`w-3.5 h-3.5 text-zinc-600 transition-transform duration-200 ${open ? "rotate-180" : ""}`}
      fill="none" stroke="currentColor" viewBox="0 0 24 24"
    >
      <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={1.5} d="M19 9l-7 7-7-7" />
    </svg>
  );
}

/* ── state snapshot table ───────────────────────────── */
function StateTable({ state, label }: { state: SmtState; label?: string }) {
  const rows: [string, string][] = [];
  if (state.status) rows.push(["status", state.status]);
  for (const [k, v] of Object.entries(state.counters ?? {})) rows.push([k, String(v)]);
  for (const [k, v] of Object.entries(state.booleans ?? {})) rows.push([k, String(v)]);
  if (rows.length === 0) return null;

  return (
    <div>
      {label && <div className="text-[10px] text-zinc-500 mb-1 uppercase tracking-wider">{label}</div>}
      <div className={`${INNER} p-2`}>
        {rows.map(([k, v]) => (
          <div key={k} className="flex justify-between gap-4 py-px">
            <span className="font-mono text-[11px] text-zinc-500">{k}</span>
            <span className="font-mono text-[11px] text-zinc-300">{v}</span>
          </div>
        ))}
      </div>
    </div>
  );
}

/* ── L0 Symbolic ────────────────────────────────────── */
function SmtPanel({ data }: { data: SmtDetails }) {
  return (
    <div className="space-y-4">
      {data.guard_satisfiability?.length > 0 && (
        <div>
          <div className="text-[11px] text-zinc-400 mb-2 uppercase tracking-wider">Guard Satisfiability</div>
          <div className={`${INNER} divide-y divide-white/[0.03]`}>
            {data.guard_satisfiability.map(([action, sat]) => (
              <div key={action} className="flex items-center justify-between px-3 py-1.5">
                <span className="font-mono text-[11px] text-zinc-300">{action}</span>
                <Dot passed={sat} />
              </div>
            ))}
          </div>
        </div>
      )}

      {data.inductive_invariants?.length > 0 && (
        <div>
          <div className="text-[11px] text-zinc-400 mb-2 uppercase tracking-wider">Inductive Invariants</div>
          <div className={`${INNER} divide-y divide-white/[0.03]`}>
            {data.inductive_invariants.map(([name, holds]) => (
              <div key={name} className="flex items-center justify-between px-3 py-1.5">
                <span className="font-mono text-[11px] text-zinc-300">{name}</span>
                <span className={`font-mono text-[10px] ${holds ? G : B}`}>
                  {holds ? "inductive" : "non-inductive"}
                </span>
              </div>
            ))}
          </div>
        </div>
      )}

      {data.unreachable_states?.length > 0 && (
        <div>
          <div className="text-[11px] text-zinc-400 mb-2 uppercase tracking-wider">Unreachable States</div>
          <div className="flex flex-wrap gap-1.5">
            {data.unreachable_states.map((s) => (
              <span key={s} className={`font-mono text-[10px] px-2 py-0.5 rounded-full ${INNER} border border-white/[0.04] text-zinc-400`}>
                {s}
              </span>
            ))}
          </div>
        </div>
      )}
    </div>
  );
}

/* ── L1 Model Check ─────────────────────────────────── */
function ModelCheckPanel({ data }: { data: ModelCheckDetails }) {
  return (
    <div className="space-y-4">
      <div className="flex gap-5 font-mono text-[11px]">
        <span className="text-zinc-500">states <span className="text-zinc-300">{data.states_explored?.toLocaleString()}</span></span>
        <span className="text-zinc-500">complete <span className={data.is_complete ? G : B}>{String(data.is_complete)}</span></span>
      </div>

      {data.counterexamples?.length > 0 && (
        <div>
          <div className="text-[11px] text-zinc-400 mb-2 uppercase tracking-wider">
            Counterexamples ({data.counterexamples.length})
          </div>
          <div className="space-y-2">
            {data.counterexamples.map((cx, i) => (
              <div key={i} className={`${GLASS} p-3 bg-gradient-to-r ${B_GRAD} to-transparent`}>
                <div className={`font-mono text-[12px] ${B} mb-2`}>{cx.property}</div>
                {cx.trace?.length > 0 && (
                  <div className="space-y-1.5 ml-1">
                    {cx.trace.map((step, j) => (
                      <div key={j} className="flex items-start gap-2.5">
                        <span className="font-mono text-[10px] text-zinc-600 w-3 text-right flex-shrink-0 pt-0.5">{j}</span>
                        <div className="flex-1"><StateTable state={step} /></div>
                      </div>
                    ))}
                  </div>
                )}
              </div>
            ))}
          </div>
        </div>
      )}

      {data.all_properties_hold && (
        <div className={`font-mono text-[11px] ${G}`}>All properties hold</div>
      )}
    </div>
  );
}

/* ── L2 Simulation sub-cards ────────────────────────── */
function InvariantViolationCard({ v }: { v: InvariantViolation }) {
  return (
    <div className={`${GLASS} p-3 bg-gradient-to-r ${B_GRAD} to-transparent`}>
      <div className="flex items-center justify-between mb-2">
        <span className={`font-mono text-[12px] ${B}`}>{v.invariant}</span>
        <span className="text-[10px] font-mono text-zinc-600">tick {v.tick}</span>
      </div>
      <div className="flex items-center gap-2 mb-3 font-mono text-[11px]">
        <span className="text-zinc-500">actor</span>
        <span className="text-zinc-300">{v.actor_id}</span>
        <span className="text-zinc-700">/</span>
        <span className="text-zinc-500">action</span>
        <span className={B}>{v.action}</span>
      </div>
      <div className="grid grid-cols-2 gap-2">
        <StateTable state={v.state_before} label="Before" />
        <StateTable state={v.state_after} label="After" />
      </div>
    </div>
  );
}

function LivenessGroupCard({ property, violations }: { property: string; violations: LivenessViolation[] }) {
  const [isOpen, setIsOpen] = useState(violations.length <= 3);

  return (
    <div className={`${GLASS} overflow-hidden bg-gradient-to-r ${B_GRAD} to-transparent`}>
      <button
        onClick={() => setIsOpen(!isOpen)}
        className="w-full flex items-center justify-between p-3 text-left hover:bg-white/[0.015] transition-colors"
      >
        <span className={`font-mono text-[12px] ${B}`}>{property}</span>
        <div className="flex items-center gap-2.5">
          <span className="text-[10px] font-mono text-zinc-500">
            {violations.length} actor{violations.length !== 1 ? "s" : ""}
          </span>
          <Chevron open={isOpen} />
        </div>
      </button>
      {isOpen && (
        <div className="border-t border-white/[0.03] divide-y divide-white/[0.03]">
          {violations.map((v, i) => (
            <div key={i} className="p-3">
              <div className="flex items-center gap-2 mb-1.5 font-mono text-[11px]">
                <span className="text-zinc-500">actor</span>
                <span className="text-zinc-300">{v.actor_id}</span>
              </div>
              {v.description && (
                <div className="text-[11px] text-zinc-400 mb-2">{v.description}</div>
              )}
              <StateTable state={v.final_state} label="Stuck State" />
            </div>
          ))}
        </div>
      )}
    </div>
  );
}

/* ── L2 Simulation ──────────────────────────────────── */
function SimulationPanel({ data }: { data: SimulationDetails }) {
  const livenessGroups: Record<string, LivenessViolation[]> = {};
  for (const v of data.liveness_violations ?? []) {
    if (!livenessGroups[v.property]) livenessGroups[v.property] = [];
    livenessGroups[v.property].push(v);
  }

  return (
    <div className="space-y-4">
      <div className="flex flex-wrap gap-x-5 gap-y-1 font-mono text-[11px]">
        <span className="text-zinc-500">ticks <span className="text-zinc-300">{data.ticks}</span></span>
        <span className="text-zinc-500">transitions <span className="text-zinc-300">{data.total_transitions}</span></span>
        <span className="text-zinc-500">messages <span className="text-zinc-300">{data.total_messages}</span></span>
        <span className="text-zinc-500">dropped <span className={data.total_dropped > 0 ? B : "text-zinc-300"}>{data.total_dropped}</span></span>
        <span className="text-zinc-500">seed <span className="text-zinc-300">{data.seed}</span></span>
      </div>

      {data.violations?.length > 0 && (
        <div>
          <div className="text-[11px] text-zinc-400 mb-2 uppercase tracking-wider">
            Invariant Violations ({data.violations.length})
          </div>
          <div className="space-y-2">
            {data.violations.map((v, i) => <InvariantViolationCard key={i} v={v} />)}
          </div>
        </div>
      )}

      {Object.keys(livenessGroups).length > 0 && (
        <div>
          <div className="text-[11px] text-zinc-400 mb-2 uppercase tracking-wider">
            Liveness Violations ({data.liveness_violations.length})
          </div>
          <div className="space-y-2">
            {Object.entries(livenessGroups).map(([prop, violations]) => (
              <LivenessGroupCard key={prop} property={prop} violations={violations} />
            ))}
          </div>
        </div>
      )}

      {data.all_invariants_held && (
        <div className={`font-mono text-[11px] ${G}`}>All invariants held</div>
      )}
    </div>
  );
}

/* ── L3 Property Test ───────────────────────────────── */
function PropTestPanel({ data }: { data: PropTestDetails }) {
  return (
    <div className="space-y-4">
      <div className="font-mono text-[11px] text-zinc-500">
        cases <span className="text-zinc-300">{data.total_cases}</span>
      </div>

      {data.failure && (
        <div className={`${GLASS} p-3 bg-gradient-to-r ${B_GRAD} to-transparent`}>
          <div className={`font-mono text-[12px] ${B} mb-3`}>{data.failure.invariant}</div>

          {data.failure.action_sequence?.length > 0 && (
            <div className="mb-3">
              <div className="text-[10px] text-zinc-500 mb-1.5 uppercase tracking-wider">Minimal Action Sequence</div>
              <div className={`${INNER} p-2 space-y-0.5`}>
                {data.failure.action_sequence.map((action, i) => (
                  <div key={i} className="flex gap-2.5 font-mono text-[11px]">
                    <span className="text-zinc-600 w-4 text-right">{i + 1}.</span>
                    <span className={B}>{action}</span>
                  </div>
                ))}
              </div>
            </div>
          )}

          {data.failure.final_state && (
            <div>
              <div className="text-[10px] text-zinc-500 mb-1.5 uppercase tracking-wider">Final State</div>
              <pre className={`${INNER} p-2 font-mono text-[11px] text-zinc-300 whitespace-pre-wrap`}>
                {data.failure.final_state}
              </pre>
            </div>
          )}
        </div>
      )}

      {data.passed && (
        <div className={`font-mono text-[11px] ${G}`}>All property tests passed</div>
      )}
    </div>
  );
}

/* ── detail panel router ────────────────────────────── */
function LevelDetailPanel({ level }: { level: VerificationLevel }) {
  if (level.smt) return <SmtPanel data={level.smt} />;
  if (level.verification) return <ModelCheckPanel data={level.verification} />;
  if (level.simulation) return <SimulationPanel data={level.simulation} />;
  if (level.prop_test) return <PropTestPanel data={level.prop_test} />;

  if (level.details) {
    return (
      <pre className={`${INNER} p-2.5 font-mono text-[11px] text-zinc-400 whitespace-pre-wrap`}>
        {level.details}
      </pre>
    );
  }

  return <div className="text-[11px] text-zinc-600 italic">No details available</div>;
}

/* ── main component ─────────────────────────────────── */
export default function CascadeResults({ levels, allPassed }: CascadeResultsProps) {
  const [expanded, setExpanded] = useState<Set<number>>(new Set());

  const toggle = (index: number) => {
    setExpanded((prev) => {
      const next = new Set(prev);
      if (next.has(index)) next.delete(index);
      else next.add(index);
      return next;
    });
  };

  return (
    <div className="space-y-2.5">
      {/* ── overall status banner ── */}
      <div className={`relative overflow-hidden ${GLASS} p-4`}>
        <div className={`absolute inset-0 bg-gradient-to-r ${allPassed ? G_GRAD : B_GRAD} to-transparent pointer-events-none`} />
        <div className="relative flex items-center gap-3.5">
          <div className={`w-8 h-8 rounded-lg flex items-center justify-center ${
            allPassed ? "bg-teal-400/10" : "bg-pink-400/10"
          }`}>
            <span className={`text-lg ${allPassed ? G : B}`}>
              {allPassed ? "\u2713" : "\u2717"}
            </span>
          </div>
          <div>
            <div className={`text-sm font-semibold tracking-tight ${allPassed ? G : B}`}>
              {allPassed ? "All Levels Passed" : "Verification Failed"}
            </div>
            <div className="text-[12px] text-zinc-500">
              {levels.filter((l) => l.passed).length} of {levels.length} levels passed
            </div>
          </div>
        </div>
      </div>

      {/* ── per-level cards ── */}
      {levels.map((level, i) => {
        const isExpanded = expanded.has(i);
        const isSkipped = level.summary.startsWith("Skipped");
        const hasDetails = !!(level.smt || level.verification || level.simulation || level.prop_test || level.details);

        return (
          <div
            key={i}
            className={`${GLASS} overflow-hidden transition-all duration-200 ${
              !level.passed && !isSkipped ? `bg-gradient-to-r ${B_GRAD} to-transparent` : ""
            }`}
          >
            <button
              onClick={() => toggle(i)}
              className="w-full flex items-center justify-between p-3 text-left hover:bg-white/[0.015] transition-colors"
            >
              <div className="flex items-center gap-3">
                <Dot passed={isSkipped ? true : level.passed} />
                <div>
                  <div className="font-mono text-[13px] text-zinc-300">{level.level}</div>
                  <div className={`text-[12px] mt-0.5 ${isSkipped ? "text-zinc-700" : "text-zinc-500"}`}>
                    {level.summary}
                  </div>
                </div>
              </div>
              <div className="flex items-center gap-3">
                {level.duration_ms !== undefined && level.duration_ms > 0 && (
                  <span className="text-[10px] font-mono text-zinc-600">{level.duration_ms}ms</span>
                )}
                {hasDetails && <Chevron open={isExpanded} />}
              </div>
            </button>

            {isExpanded && (
              <div className="px-3 pb-3 border-t border-white/[0.03]">
                <div className="mt-3">
                  <LevelDetailPanel level={level} />
                </div>
              </div>
            )}
          </div>
        );
      })}
    </div>
  );
}
