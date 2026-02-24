"use client";

import { useState, useMemo, useCallback, useEffect } from "react";
import { fetchWasmModules, fetchWasmInvocations } from "@/lib/api";
import { usePolling, useRelativeTime } from "@/lib/hooks";
import type { WasmModulesResponse, WasmInvocationsResponse } from "@/lib/types";
import ErrorDisplay from "@/components/ErrorDisplay";

function StatCard({ label, value, color }: { label: string; value: string | number; color?: string }) {
  return (
    <div className="glass rounded p-3.5">
      <div className="text-[12px] text-zinc-600">{label}</div>
      <div className={`text-4xl font-bold font-mono mt-0.5 ${color ?? "text-zinc-100"}`}>
        {value}
      </div>
    </div>
  );
}

function rateColor(rate: number): string {
  if (rate >= 80) return "text-teal-400";
  if (rate >= 50) return "text-amber-400";
  return "text-pink-400";
}

function rateBgColor(rate: number): string {
  if (rate >= 80) return "bg-teal-400";
  if (rate >= 50) return "bg-amber-400";
  return "bg-pink-400";
}

export default function IntegrationsPage() {
  const [initialLoading, setInitialLoading] = useState(true);
  const [initialError, setInitialError] = useState<string | null>(null);
  const [moduleFilter, setModuleFilter] = useState<string>("all");

  const loadInitial = useCallback(async () => {
    setInitialLoading(true);
    setInitialError(null);
    try {
      await Promise.all([fetchWasmModules(), fetchWasmInvocations()]);
    } catch (err) {
      setInitialError(err instanceof Error ? err.message : "Failed to load integrations data");
    } finally {
      setInitialLoading(false);
    }
  }, []);

  useEffect(() => {
    loadInitial();
  }, [loadInitial]);

  const modulesPoll = usePolling<WasmModulesResponse>({
    fetcher: fetchWasmModules,
    interval: 5000,
    enabled: !initialLoading && !initialError,
  });

  const invocationsPoll = usePolling<WasmInvocationsResponse>({
    fetcher: () =>
      fetchWasmInvocations(
        moduleFilter !== "all" ? { module_name: moduleFilter, limit: 100 } : { limit: 100 },
      ),
    interval: 5000,
    enabled: !initialLoading && !initialError,
  });

  const modules = modulesPoll.data;
  const invocations = invocationsPoll.data;
  const lastUpdated = useRelativeTime(modulesPoll.lastUpdated);

  // Derive stats
  const totalModules = modules?.total ?? 0;
  const totalInvocations = useMemo(() => {
    if (!modules?.modules) return 0;
    return modules.modules.reduce((sum, m) => sum + m.total_invocations, 0);
  }, [modules]);

  const overallSuccessRate = useMemo(() => {
    if (!modules?.modules || totalInvocations === 0) return 0;
    const totalSuccess = modules.modules.reduce((sum, m) => sum + m.success_count, 0);
    return Math.round((totalSuccess / totalInvocations) * 100);
  }, [modules, totalInvocations]);

  // Derive module names for filter
  const moduleNames = useMemo(() => {
    if (!modules?.modules) return [];
    return modules.modules.map((m) => m.module_name).sort();
  }, [modules]);

  if (initialLoading) {
    return (
      <div className="animate-pulse">
        <div className="h-6 bg-zinc-800/60 rounded w-40 mb-1.5" />
        <div className="h-3.5 bg-zinc-800/40 rounded w-72 mb-6" />
        <div className="grid grid-cols-3 gap-3 mb-6">
          {[0, 1, 2].map((i) => (
            <div key={i} className="glass rounded p-3.5">
              <div className="h-3 bg-zinc-800/50 rounded w-20 mb-2" />
              <div className="h-8 bg-zinc-800/50 rounded w-10" />
            </div>
          ))}
        </div>
      </div>
    );
  }

  if (initialError) {
    return <ErrorDisplay title="Cannot load integrations" message={initialError} retry={loadInitial} />;
  }

  return (
    <div className="animate-fade-in">
      {/* Header */}
      <div className="flex items-center justify-between mb-6">
        <div>
          <h1 className="text-2xl font-bold text-zinc-100 tracking-tight font-display">Integrations</h1>
          <p className="text-sm text-zinc-600 mt-0.5">
            WASM modules, invocation history, and success rates
          </p>
        </div>
        <div className="flex items-center gap-3">
          {moduleNames.length > 0 && (
            <select
              value={moduleFilter}
              onChange={(e) => setModuleFilter(e.target.value)}
              className="bg-[#111115] text-zinc-400 text-xs rounded-sm px-2 py-1.5 focus:outline-none"
            >
              <option value="all">All modules</option>
              {moduleNames.map((m) => (
                <option key={m} value={m}>{m}</option>
              ))}
            </select>
          )}
          {lastUpdated && (
            <span className="text-xs text-zinc-600">Updated {lastUpdated}</span>
          )}
        </div>
      </div>

      {/* Stats row */}
      <div className="grid grid-cols-3 gap-3 mb-6">
        <StatCard label="Total Modules" value={totalModules} />
        <StatCard label="Total Invocations" value={totalInvocations} />
        <StatCard
          label="Success Rate"
          value={totalInvocations > 0 ? `${overallSuccessRate}%` : "–"}
          color={totalInvocations > 0 ? rateColor(overallSuccessRate) : undefined}
        />
      </div>

      {/* Modules Table */}
      {modules && modules.modules.length > 0 && (
        <div className="mb-6">
          <h2 className="text-base font-semibold text-zinc-200 mb-3 tracking-tight">Modules</h2>
          <div className="glass rounded overflow-hidden">
            <table className="w-full text-[13px]">
              <thead className="sticky top-0 bg-[#111115]/90 backdrop-blur-sm z-10">
                <tr className="border-b border-white/[0.06]">
                  <th className="text-left px-3.5 py-2.5 text-zinc-600 font-medium text-xs uppercase tracking-wider">Name</th>
                  <th className="text-left px-3.5 py-2.5 text-zinc-600 font-medium text-xs uppercase tracking-wider">Hash</th>
                  <th className="text-center px-3.5 py-2.5 text-zinc-600 font-medium text-xs uppercase tracking-wider">Cached</th>
                  <th className="text-right px-3.5 py-2.5 text-zinc-600 font-medium text-xs uppercase tracking-wider">Invocations</th>
                  <th className="text-left px-3.5 py-2.5 text-zinc-600 font-medium text-xs uppercase tracking-wider w-32">Success Rate</th>
                  <th className="text-left px-3.5 py-2.5 text-zinc-600 font-medium text-xs uppercase tracking-wider">Last Used</th>
                </tr>
              </thead>
              <tbody>
                {modules.modules.map((mod_, i) => {
                  const rate = mod_.total_invocations > 0
                    ? Math.round(mod_.success_rate * 100)
                    : 0;
                  return (
                    <tr
                      key={mod_.module_name}
                      className={`border-b border-white/[0.03] ${i % 2 === 1 ? "bg-white/[0.01]" : ""}`}
                    >
                      <td className="px-3.5 py-2.5 font-mono text-zinc-300">{mod_.module_name}</td>
                      <td className="px-3.5 py-2.5 font-mono text-zinc-500 text-[11px]">
                        {mod_.sha256_hash.substring(0, 12)}...
                      </td>
                      <td className="px-3.5 py-2.5 text-center">
                        {mod_.cached ? (
                          <span className="text-[10px] font-medium bg-teal-500/15 text-teal-400 px-1.5 py-0.5 rounded">
                            cached
                          </span>
                        ) : (
                          <span className="text-[10px] font-medium bg-zinc-500/15 text-zinc-500 px-1.5 py-0.5 rounded">
                            cold
                          </span>
                        )}
                      </td>
                      <td className="px-3.5 py-2.5 text-right font-mono text-zinc-400">
                        {mod_.total_invocations}
                      </td>
                      <td className="px-3.5 py-2.5">
                        {mod_.total_invocations > 0 ? (
                          <div className="flex items-center gap-2">
                            <div className="flex-1 h-1.5 bg-white/[0.04] rounded-full overflow-hidden">
                              <div
                                className={`h-full rounded-full ${rateBgColor(rate)}`}
                                style={{ width: `${rate}%` }}
                              />
                            </div>
                            <span className={`text-[11px] font-mono ${rateColor(rate)}`}>
                              {rate}%
                            </span>
                          </div>
                        ) : (
                          <span className="text-[11px] text-zinc-600">–</span>
                        )}
                      </td>
                      <td className="px-3.5 py-2.5 text-[11px] text-zinc-500 font-mono">
                        {mod_.last_invoked_at
                          ? new Date(mod_.last_invoked_at).toLocaleTimeString()
                          : "–"}
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        </div>
      )}

      {/* Empty state for modules */}
      {modules && modules.modules.length === 0 && (
        <div className="glass rounded p-8 text-center mb-6">
          <div className="text-zinc-500 text-sm">No WASM modules uploaded yet.</div>
          <p className="text-zinc-600 text-xs mt-1">
            Upload modules via POST /observe/wasm/modules/:name
          </p>
        </div>
      )}

      {/* Recent Invocations */}
      <div>
        <h2 className="text-base font-semibold text-zinc-200 mb-3 tracking-tight">
          Recent Invocations
          {invocations && invocations.total > 0 && (
            <span className="text-zinc-600 font-normal text-[13px] ml-2">{invocations.total}</span>
          )}
        </h2>
        {!invocations || invocations.invocations.length === 0 ? (
          <div className="glass rounded p-6 text-center">
            <p className="text-sm text-zinc-500">No invocations recorded yet.</p>
          </div>
        ) : (
          <div className="glass rounded overflow-hidden max-h-96 overflow-y-auto">
            {invocations.invocations.map((inv, i) => {
              const ts = new Date(inv.timestamp);
              const timeStr = ts.toLocaleTimeString();
              return (
                <div
                  key={`${inv.timestamp}-${i}`}
                  className="flex items-center gap-3 px-3.5 py-2.5 border-b border-white/[0.03] last:border-b-0"
                >
                  <span className="text-[11px] text-zinc-600 font-mono flex-shrink-0 w-20">
                    {timeStr}
                  </span>
                  <span className="font-mono text-[11px] text-zinc-300 flex-shrink-0">
                    {inv.module_name}
                  </span>
                  <span className="text-[10px] font-mono text-zinc-600 flex-shrink-0">
                    {inv.entity_type}/{inv.entity_id}
                  </span>
                  <span className="text-[11px] text-zinc-500 font-mono flex-shrink-0">
                    {inv.trigger_action}
                  </span>
                  {inv.callback_action && (
                    <>
                      <span className="text-zinc-700 text-[11px]">&rarr;</span>
                      <span className="text-[11px] text-teal-400 font-mono flex-shrink-0">
                        {inv.callback_action}
                      </span>
                    </>
                  )}
                  <span className="ml-auto flex-shrink-0">
                    {inv.success ? (
                      <span className="text-[10px] font-medium bg-teal-500/15 text-teal-400 px-1.5 py-0.5 rounded">
                        ok
                      </span>
                    ) : (
                      <span className="text-[10px] font-medium bg-pink-500/15 text-pink-400 px-1.5 py-0.5 rounded">
                        fail
                      </span>
                    )}
                  </span>
                  <span className="text-[10px] font-mono text-zinc-600 flex-shrink-0 w-12 text-right">
                    {inv.duration_ms}ms
                  </span>
                  {inv.error && (
                    <span className="text-[11px] text-pink-400 truncate max-w-48" title={inv.error}>
                      {inv.error}
                    </span>
                  )}
                </div>
              );
            })}
          </div>
        )}
      </div>
    </div>
  );
}
