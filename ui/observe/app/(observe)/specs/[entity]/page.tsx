"use client";

import { useEffect, useState, useCallback } from "react";
import { useParams } from "next/navigation";
import Link from "next/link";
import { fetchSpecDetail } from "@/lib/api";
import type { SpecDetail } from "@/lib/types";
import StateMachineGraph from "@/components/StateMachineGraph";
import ErrorDisplay from "@/components/ErrorDisplay";

function SpecSkeleton() {
  return (
    <div className="animate-pulse">
      <div className="h-3.5 bg-zinc-800/40 rounded w-44 mb-1.5" />
      <div className="h-6 bg-zinc-800/60 rounded w-52 mb-5" />
      <div className="h-4 bg-zinc-800/50 rounded w-28 mb-2.5" />
      <div className="bg-[#111115] rounded-lg h-56 mb-6" />
      <div className="h-4 bg-zinc-800/50 rounded w-24 mb-2.5" />
      <div className="bg-[#111115] rounded-lg h-44" />
    </div>
  );
}

export default function SpecViewer() {
  const params = useParams();
  const entity = params.entity as string;
  const [spec, setSpec] = useState<SpecDetail | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const data = await fetchSpecDetail(entity);
      setSpec(data);
    } catch (err) {
      setError(err instanceof Error ? err.message : `Failed to load spec "${entity}"`);
    } finally {
      setLoading(false);
    }
  }, [entity]);

  useEffect(() => {
    load();
  }, [load]);

  if (loading) return <SpecSkeleton />;
  if (error) {
    return (
      <ErrorDisplay
        title={`Spec not found: ${entity}`}
        message={error}
        retry={load}
      />
    );
  }
  if (!spec) {
    return (
      <ErrorDisplay
        title="Spec not found"
        message={`No spec found for entity type "${entity}".`}
      />
    );
  }

  return (
    <div className="animate-fade-in">
      {/* Header */}
      <div className="flex items-center justify-between mb-5">
        <div>
          <div className="flex items-center gap-1.5 text-[12px] text-zinc-600 mb-1">
            <Link href="/" className="hover:text-zinc-400 transition-colors">Dashboard</Link>
            <span>/</span>
            <span className="text-zinc-400">Specs</span>
            <span>/</span>
            <span className="text-zinc-400">{spec.entity_type}</span>
          </div>
          <h1 className="text-2xl font-bold text-zinc-100 tracking-tight font-display">{spec.entity_type} Spec</h1>
        </div>
        <Link
          href={`/verify/${spec.entity_type}`}
          className="px-3.5 py-1.5 bg-teal-500 hover:bg-teal-400 text-white text-[13px] rounded-md transition-colors"
        >
          Run Verification
        </Link>
      </div>

      {/* State Machine Diagram */}
      <div className="mb-6">
        <h2 className="text-base font-semibold text-zinc-200 mb-2.5 tracking-tight">State Machine</h2>
        <StateMachineGraph spec={spec} />
        <div className="flex gap-5 mt-2.5 text-[11px] text-zinc-600">
          <div className="flex items-center gap-1.5">
            <div className="w-2.5 h-2.5 rounded border border-teal-500/50 bg-teal-500/10" />
            <span>Initial state</span>
          </div>
          <div className="flex items-center gap-1.5">
            <div className="w-2.5 h-2.5 rounded border border-zinc-700 bg-zinc-900" />
            <span>Normal state</span>
          </div>
          <div className="flex items-center gap-1.5">
            <div className="w-2.5 h-2.5 rounded border border-dashed border-zinc-600 bg-zinc-900" />
            <span>Terminal state</span>
          </div>
        </div>
      </div>

      {/* Transition Table */}
      <div className="mb-6">
        <h2 className="text-base font-semibold text-zinc-200 mb-2.5 tracking-tight">Transitions</h2>
        <div className="bg-[#111115] rounded-lg overflow-hidden">
          <table className="w-full text-[13px]">
            <thead>
              <tr className="border-b border-white/[0.06]">
                <th className="text-left px-3.5 py-2.5 text-zinc-600 font-medium text-[11px] uppercase tracking-wider">Action</th>
                <th className="text-left px-3.5 py-2.5 text-zinc-600 font-medium text-[11px] uppercase tracking-wider">Kind</th>
                <th className="text-left px-3.5 py-2.5 text-zinc-600 font-medium text-[11px] uppercase tracking-wider">From</th>
                <th className="text-left px-3.5 py-2.5 text-zinc-600 font-medium text-[11px] uppercase tracking-wider">To</th>
                <th className="text-left px-3.5 py-2.5 text-zinc-600 font-medium text-[11px] uppercase tracking-wider">Guard</th>
                <th className="text-left px-3.5 py-2.5 text-zinc-600 font-medium text-[11px] uppercase tracking-wider">Effect</th>
              </tr>
            </thead>
            <tbody>
              {spec.actions.map((action, i) => (
                <tr
                  key={i}
                  className="border-b border-white/[0.03] hover:bg-white/[0.02] transition-colors"
                >
                  <td className="px-3.5 py-2 font-mono text-teal-400">{action.name}</td>
                  <td className="px-3.5 py-2">
                    <span
                      className={`text-[10px] font-mono px-1.5 py-0.5 rounded-full ${
                        action.kind === "input"
                          ? "bg-teal-500/10 text-teal-400"
                          : "bg-violet-500/10 text-violet-400"
                      }`}
                    >
                      {action.kind}
                    </span>
                  </td>
                  <td className="px-3.5 py-2 font-mono text-zinc-400">{action.from.join(", ")}</td>
                  <td className="px-3.5 py-2 font-mono text-zinc-400">{action.to ?? <span className="text-zinc-700">--</span>}</td>
                  <td className="px-3.5 py-2 font-mono text-yellow-400/70 text-[11px]">
                    {action.guards.length > 0 ? action.guards.join("; ") : <span className="text-zinc-700">--</span>}
                  </td>
                  <td className="px-3.5 py-2 font-mono text-zinc-500 text-[11px]">
                    {action.effects.length > 0 ? action.effects.join("; ") : <span className="text-zinc-700">--</span>}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      </div>

      {/* Invariants */}
      <div className="mb-6">
        <h2 className="text-base font-semibold text-zinc-200 mb-2.5 tracking-tight">Invariants</h2>
        {spec.invariants.length === 0 ? (
          <div className="bg-[#111115] rounded-lg p-6 text-center">
            <p className="text-[13px] text-zinc-500">No invariants defined.</p>
          </div>
        ) : (
          <div className="space-y-1.5">
            {spec.invariants.map((inv, i) => (
              <div
                key={i}
                className="bg-[#111115] rounded-lg p-3"
              >
                <div className="flex items-center gap-2 mb-1.5">
                  <span className="text-[13px] font-semibold text-zinc-200">{inv.name}</span>
                </div>
                <div className="text-[13px] space-y-0.5">
                  <div className="flex gap-2">
                    <span className="text-zinc-600 w-14 flex-shrink-0 text-[11px]">when</span>
                    <code className="font-mono text-yellow-400/70 text-[11px]">{inv.when.length > 0 ? inv.when.join(", ") : "always"}</code>
                  </div>
                  <div className="flex gap-2">
                    <span className="text-zinc-600 w-14 flex-shrink-0 text-[11px]">assert</span>
                    <code className="font-mono text-teal-400/70 text-[11px]">{inv.assertion}</code>
                  </div>
                </div>
              </div>
            ))}
          </div>
        )}
      </div>

      {/* State Variables */}
      <div className="mb-6">
        <h2 className="text-base font-semibold text-zinc-200 mb-2.5 tracking-tight">State Variables</h2>
        {spec.state_variables.length === 0 ? (
          <div className="bg-[#111115] rounded-lg p-6 text-center">
            <p className="text-[13px] text-zinc-500">No state variables defined.</p>
          </div>
        ) : (
          <div className="bg-[#111115] rounded-lg overflow-hidden">
            <table className="w-full text-[13px]">
              <thead>
                <tr className="border-b border-white/[0.06]">
                  <th className="text-left px-3.5 py-2.5 text-zinc-600 font-medium text-[11px] uppercase tracking-wider">Name</th>
                  <th className="text-left px-3.5 py-2.5 text-zinc-600 font-medium text-[11px] uppercase tracking-wider">Type</th>
                  <th className="text-left px-3.5 py-2.5 text-zinc-600 font-medium text-[11px] uppercase tracking-wider">Initial Value</th>
                </tr>
              </thead>
              <tbody>
                {spec.state_variables.map((v, i) => (
                  <tr
                    key={i}
                    className="border-b border-white/[0.03] hover:bg-white/[0.02] transition-colors"
                  >
                    <td className="px-3.5 py-2 font-mono text-zinc-200">{v.name}</td>
                    <td className="px-3.5 py-2 font-mono text-purple-400 text-[11px]">{v.var_type}</td>
                    <td className="px-3.5 py-2 font-mono text-zinc-500 text-[11px]">{v.initial}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </div>
    </div>
  );
}
