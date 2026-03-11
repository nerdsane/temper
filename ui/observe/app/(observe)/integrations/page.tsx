"use client";

import { useState, useMemo, useCallback, useEffect } from "react";
import { fetchWasmModules, fetchWasmInvocations } from "@/lib/api";
import { usePolling, useRelativeTime } from "@/lib/hooks";
import type { WasmModulesResponse, WasmInvocationsResponse } from "@/lib/types";
import ErrorDisplay from "@/components/ErrorDisplay";
import StatCard from "@/components/StatCard";
import { rateColor, rateBgColor } from "@/lib/utils";

export default function IntegrationsPage() {
  const [initialLoading, setInitialLoading] = useState(true);
  const [initialError, setInitialError] = useState<string | null>(null);
  const [moduleFilter, setModuleFilter] = useState<string>("all");
  const [expandedInvocation, setExpandedInvocation] = useState<number | null>(null);

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
        <div className="h-6 bg-[var(--color-border)] rounded w-40 mb-1.5" />
        <div className="h-3.5 bg-[var(--color-border)] rounded w-72 mb-6" />
        <div className="grid grid-cols-3 gap-3 mb-6">
          {[0, 1, 2].map((i) => (
            <div key={i} className="glass rounded-[2px] p-4">
              <div className="h-3 bg-[var(--color-border)] rounded w-20 mb-2" />
              <div className="h-8 bg-[var(--color-border)] rounded w-10" />
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
          <h1 className="text-2xl text-[var(--color-text-primary)] tracking-tight font-serif">Integrations</h1>
          <p className="text-sm text-[var(--color-text-muted)] mt-0.5">
            WASM modules, invocation history, and success rates
          </p>
        </div>
        <div className="flex items-center gap-3">
          {moduleNames.length > 0 && (
            <select
              value={moduleFilter}
              onChange={(e) => setModuleFilter(e.target.value)}
              className="bg-[var(--color-bg-surface)] text-[var(--color-text-secondary)] text-xs rounded-sm px-2 py-1.5 focus:outline-none"
            >
              <option value="all">All modules</option>
              {moduleNames.map((m) => (
                <option key={m} value={m}>{m}</option>
              ))}
            </select>
          )}
          {lastUpdated && (
            <span className="text-xs text-[var(--color-text-muted)]">Updated {lastUpdated}</span>
          )}
        </div>
      </div>

      {/* Stats row */}
      <div className="grid grid-cols-3 gap-3 mb-6">
        <StatCard label="Total Modules" value={totalModules} />
        <StatCard label="Total Invocations" value={totalInvocations} />
        <StatCard
          label="Success Rate"
          value={totalInvocations > 0 ? `${overallSuccessRate}%` : "\u2013"}
          color={totalInvocations > 0 ? rateColor(overallSuccessRate) : undefined}
        />
      </div>

      {/* Modules Table */}
      {modules && modules.modules.length > 0 && (
        <div className="mb-6">
          <h2 className="text-base font-semibold text-[var(--color-text-primary)] mb-3 tracking-tight">Modules</h2>
          <div className="glass rounded overflow-hidden">
            <table className="w-full text-[13px]">
              <thead className="sticky top-0 bg-[color-mix(in_srgb,var(--color-bg-surface)_90%,transparent)] backdrop-blur-sm z-10">
                <tr className="border-b border-[var(--color-border)]">
                  <th className="text-left px-3.5 py-2.5 text-[var(--color-text-muted)] font-medium text-xs uppercase tracking-wider">Tenant</th>
                  <th className="text-left px-3.5 py-2.5 text-[var(--color-text-muted)] font-medium text-xs uppercase tracking-wider">Name</th>
                  <th className="text-left px-3.5 py-2.5 text-[var(--color-text-muted)] font-medium text-xs uppercase tracking-wider">Hash</th>
                  <th className="text-center px-3.5 py-2.5 text-[var(--color-text-muted)] font-medium text-xs uppercase tracking-wider">Cached</th>
                  <th className="text-right px-3.5 py-2.5 text-[var(--color-text-muted)] font-medium text-xs uppercase tracking-wider">Invocations</th>
                  <th className="text-left px-3.5 py-2.5 text-[var(--color-text-muted)] font-medium text-xs uppercase tracking-wider w-32">Success Rate</th>
                  <th className="text-left px-3.5 py-2.5 text-[var(--color-text-muted)] font-medium text-xs uppercase tracking-wider">Last Used</th>
                </tr>
              </thead>
              <tbody>
                {modules.modules.map((mod_, i) => {
                  const rate = mod_.total_invocations > 0
                    ? Math.round(mod_.success_rate * 100)
                    : 0;
                  return (
                    <tr
                      key={`${mod_.tenant}-${mod_.module_name}`}
                      className={`border-b border-[var(--color-border)] ${i % 2 === 1 ? "bg-[var(--color-bg-elevated)]" : ""}`}
                    >
                      <td className="px-3.5 py-2.5 text-[11px] text-[var(--color-text-secondary)]">{mod_.tenant}</td>
                      <td className="px-3.5 py-2.5 font-mono text-[var(--color-text-secondary)]">{mod_.module_name}</td>
                      <td className="px-3.5 py-2.5 font-mono text-[var(--color-text-secondary)] text-[11px]">
                        {mod_.sha256_hash.substring(0, 12)}...
                      </td>
                      <td className="px-3.5 py-2.5 text-center">
                        {mod_.cached ? (
                          <span className="text-[10px] font-medium bg-[var(--color-accent-teal-dim)] text-[var(--color-accent-teal)] px-1.5 py-0.5 rounded">
                            cached
                          </span>
                        ) : (
                          <span className="text-[10px] font-medium bg-[var(--color-accent-lime-dim)] text-[var(--color-text-secondary)] px-1.5 py-0.5 rounded">
                            cold
                          </span>
                        )}
                      </td>
                      <td className="px-3.5 py-2.5 text-right font-mono text-[var(--color-text-secondary)]">
                        {mod_.total_invocations}
                      </td>
                      <td className="px-3.5 py-2.5">
                        {mod_.total_invocations > 0 ? (
                          <div className="flex items-center gap-2">
                            <div className="flex-1 h-1.5 bg-[var(--color-bg-elevated)] rounded-full overflow-hidden">
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
                          <span className="text-[11px] text-[var(--color-text-muted)]">{"\u2013"}</span>
                        )}
                      </td>
                      <td className="px-3.5 py-2.5 text-[11px] text-[var(--color-text-secondary)] font-mono">
                        {mod_.last_invoked_at
                          ? new Date(mod_.last_invoked_at).toLocaleTimeString()
                          : "\u2013"}
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
        <div className="glass rounded-[2px] p-6 text-center mb-6">
          <div className="text-[var(--color-text-secondary)] text-sm">No WASM modules uploaded yet.</div>
          <p className="text-[var(--color-text-muted)] text-xs mt-1">
            Upload modules via POST /api/wasm/modules/:name
          </p>
        </div>
      )}

      {/* Recent Invocations */}
      <div>
        <h2 className="text-base font-semibold text-[var(--color-text-primary)] mb-3 tracking-tight">
          Recent Invocations
          {invocations && invocations.total > 0 && (
            <span className="text-[var(--color-text-muted)] font-normal text-[13px] ml-2">{invocations.total}</span>
          )}
        </h2>
        {!invocations || invocations.invocations.length === 0 ? (
          <div className="glass rounded-[2px] p-6 text-center">
            <p className="text-sm text-[var(--color-text-secondary)]">No invocations recorded yet.</p>
          </div>
        ) : (
          <div className="glass rounded overflow-hidden max-h-96 overflow-y-auto">
            {invocations.invocations.map((inv, i) => {
              const ts = new Date(inv.timestamp);
              const timeStr = ts.toLocaleTimeString();
              const isExpanded = expandedInvocation === i;
              const hasError = !inv.success && inv.error;
              return (
                <div key={`${inv.timestamp}-${i}`}>
                  <div
                    className={`flex items-center gap-3 px-3.5 py-2.5 border-b border-[var(--color-border)] last:border-b-0 ${hasError ? "cursor-pointer hover:bg-[var(--color-bg-elevated)]" : ""}`}
                    onClick={() => hasError && setExpandedInvocation(isExpanded ? null : i)}
                  >
                    <span className="text-[11px] text-[var(--color-text-muted)] font-mono flex-shrink-0 w-20">
                      {timeStr}
                    </span>
                    <span className="font-mono text-[11px] text-[var(--color-text-secondary)] flex-shrink-0">
                      {inv.module_name}
                    </span>
                    <span className="text-[10px] font-mono text-[var(--color-text-muted)] flex-shrink-0">
                      {inv.entity_type}/{inv.entity_id.substring(0, 8)}
                    </span>
                    <span className="text-[11px] text-[var(--color-text-secondary)] font-mono flex-shrink-0">
                      {inv.trigger_action}
                    </span>
                    {inv.callback_action && (
                      <>
                        <span className="text-[var(--color-text-muted)] text-[11px]">&rarr;</span>
                        <span className={`text-[11px] font-mono flex-shrink-0 ${inv.success ? "text-[var(--color-accent-teal)]" : "text-[var(--color-accent-pink)]"}`}>
                          {inv.callback_action}
                        </span>
                      </>
                    )}
                    <span className="ml-auto flex-shrink-0">
                      {inv.success ? (
                        <span className="text-[10px] font-medium bg-[var(--color-accent-teal-dim)] text-[var(--color-accent-teal)] px-1.5 py-0.5 rounded">
                          ok
                        </span>
                      ) : (
                        <span className="text-[10px] font-medium bg-[var(--color-accent-pink-dim)] text-[var(--color-accent-pink)] px-1.5 py-0.5 rounded">
                          fail
                        </span>
                      )}
                    </span>
                    <span className="text-[10px] font-mono text-[var(--color-text-muted)] flex-shrink-0 w-12 text-right">
                      {inv.duration_ms}ms
                    </span>
                    {hasError && (
                      <span className="text-[11px] text-[var(--color-text-muted)] flex-shrink-0">
                        {isExpanded ? "\u25B4" : "\u25BE"}
                      </span>
                    )}
                  </div>
                  {isExpanded && hasError && (
                    <div className="px-3.5 py-3 bg-[var(--color-accent-pink-dim)] border-b border-[var(--color-border)]">
                      <div className="text-[10px] text-[var(--color-text-muted)] uppercase tracking-wider mb-1.5">Error Details</div>
                      <pre className="text-[12px] text-[var(--color-accent-pink)] font-mono whitespace-pre-wrap break-all leading-relaxed">
                        {inv.error}
                      </pre>
                      <div className="mt-2 flex gap-4 text-[10px] text-[var(--color-text-muted)]">
                        <span>Tenant: {inv.tenant}</span>
                        <span>Entity: {inv.entity_type}/{inv.entity_id}</span>
                        <span>Trigger: {inv.trigger_action}</span>
                        <span>{ts.toLocaleString()}</span>
                      </div>
                    </div>
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
