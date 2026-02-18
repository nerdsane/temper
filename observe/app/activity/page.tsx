"use client";

import { useEffect, useState, useMemo, useCallback, useRef } from "react";
import { fetchTrajectories, subscribeEntityEvents } from "@/lib/api";
import { usePolling, useRelativeTime } from "@/lib/hooks";
import type { TrajectoryResponse, EntityStateChange } from "@/lib/types";
import ErrorDisplay from "@/components/ErrorDisplay";

function StatCard({ label, value, color }: { label: string; value: string | number; color?: string }) {
  return (
    <div className="bg-white/[0.025] backdrop-blur-xl border border-white/[0.04] rounded-xl p-3.5">
      <div className="text-[12px] text-zinc-600">{label}</div>
      <div className={`text-2xl font-semibold font-mono mt-0.5 ${color ?? "text-zinc-100"}`}>
        {value}
      </div>
    </div>
  );
}

function rateColor(rate: number): string {
  if (rate >= 80) return "text-emerald-400";
  if (rate >= 50) return "text-amber-400";
  return "text-rose-400";
}

function rateBgColor(rate: number): string {
  if (rate >= 80) return "bg-emerald-400";
  if (rate >= 50) return "bg-amber-400";
  return "bg-rose-400";
}

export default function ActivityPage() {
  const [initialLoading, setInitialLoading] = useState(true);
  const [initialError, setInitialError] = useState<string | null>(null);
  const [liveCount, setLiveCount] = useState(0);
  const [filterType, setFilterType] = useState<string>("all");
  const liveCountRef = useRef(liveCount);
  liveCountRef.current = liveCount;

  const loadInitial = useCallback(async () => {
    setInitialLoading(true);
    setInitialError(null);
    try {
      await fetchTrajectories();
    } catch (err) {
      setInitialError(err instanceof Error ? err.message : "Failed to load activity data");
    } finally {
      setInitialLoading(false);
    }
  }, []);

  useEffect(() => {
    loadInitial();
  }, [loadInitial]);

  const trajectoryPoll = usePolling<TrajectoryResponse>({
    fetcher: () => fetchTrajectories(filterType !== "all" ? { entity_type: filterType } : undefined),
    interval: 5000,
    enabled: !initialLoading && !initialError,
  });

  const data = trajectoryPoll.data;
  const lastUpdated = useRelativeTime(trajectoryPoll.lastUpdated);

  // Live event counter
  useEffect(() => {
    if (initialLoading || initialError) return;
    const cleanup = subscribeEntityEvents(() => {
      setLiveCount((c) => c + 1);
    });
    return cleanup;
  }, [initialLoading, initialError]);

  // Derive action names for filter
  const actionNames = useMemo(() => {
    if (!data?.by_action) return [];
    return Object.keys(data.by_action).sort();
  }, [data]);

  // Derive entity types from failed intents
  const entityTypes = useMemo(() => {
    if (!data?.failed_intents) return [];
    const set = new Set<string>();
    for (const fi of data.failed_intents) {
      if (fi.entity_type) set.add(fi.entity_type);
    }
    return Array.from(set).sort();
  }, [data]);

  if (initialLoading) {
    return (
      <div className="animate-pulse">
        <div className="h-6 bg-zinc-800/60 rounded w-36 mb-1.5" />
        <div className="h-3.5 bg-zinc-800/40 rounded w-64 mb-6" />
        <div className="grid grid-cols-4 gap-3 mb-6">
          {[0, 1, 2, 3].map((i) => (
            <div key={i} className="bg-[#0a0a0f] border border-white/[0.06] rounded-lg p-3.5">
              <div className="h-3 bg-zinc-800/50 rounded w-20 mb-2" />
              <div className="h-8 bg-zinc-800/50 rounded w-10" />
            </div>
          ))}
        </div>
      </div>
    );
  }

  if (initialError) {
    return <ErrorDisplay title="Cannot load activity" message={initialError} retry={loadInitial} />;
  }

  const successRate = data ? Math.round(data.success_rate * 100) : 0;

  return (
    <div className="animate-fade-in">
      {/* Header */}
      <div className="flex items-center justify-between mb-6">
        <div>
          <h1 className="text-xl font-semibold text-zinc-100 tracking-tight">Activity</h1>
          <p className="text-[13px] text-zinc-600 mt-0.5">
            Trajectory stats, action breakdown, and failed intents
          </p>
        </div>
        <div className="flex items-center gap-3">
          {liveCount > 0 && (
            <div className="flex items-center gap-1.5">
              <div className="w-1.5 h-1.5 bg-emerald-400 rounded-full animate-pulse" />
              <span className="text-[11px] text-zinc-500 font-mono">{liveCount} live</span>
            </div>
          )}
          {entityTypes.length > 0 && (
            <select
              value={filterType}
              onChange={(e) => setFilterType(e.target.value)}
              className="bg-[#0a0a0f] border border-white/[0.06] text-zinc-400 text-[11px] rounded-md px-2 py-1.5 focus:border-blue-500/50 focus:outline-none"
            >
              <option value="all">All types</option>
              {entityTypes.map((t) => (
                <option key={t} value={t}>{t}</option>
              ))}
            </select>
          )}
          {lastUpdated && (
            <span className="text-[11px] text-zinc-600">Updated {lastUpdated}</span>
          )}
        </div>
      </div>

      {/* Stats row */}
      <div className="grid grid-cols-4 gap-3 mb-6">
        <StatCard label="Total Transitions" value={data?.total ?? 0} />
        <StatCard
          label="Success Rate"
          value={`${successRate}%`}
          color={data ? rateColor(successRate) : undefined}
        />
        <StatCard label="Errors" value={data?.error_count ?? 0} color={data && data.error_count > 0 ? "text-rose-400" : undefined} />
        <StatCard label="Actions Tracked" value={actionNames.length} />
      </div>

      {/* Action Breakdown */}
      {data && actionNames.length > 0 && (
        <div className="mb-6">
          <h2 className="text-[15px] font-semibold text-zinc-200 mb-3 tracking-tight">Action Breakdown</h2>
          <div className="bg-[#0a0a0f] border border-white/[0.06] rounded-lg overflow-hidden">
            <table className="w-full text-[13px]">
              <thead>
                <tr className="border-b border-white/[0.06]">
                  <th className="text-left px-3.5 py-2.5 text-zinc-600 font-medium text-[11px] uppercase tracking-wider">Action</th>
                  <th className="text-right px-3.5 py-2.5 text-zinc-600 font-medium text-[11px] uppercase tracking-wider">Total</th>
                  <th className="text-right px-3.5 py-2.5 text-zinc-600 font-medium text-[11px] uppercase tracking-wider">Success</th>
                  <th className="text-right px-3.5 py-2.5 text-zinc-600 font-medium text-[11px] uppercase tracking-wider">Errors</th>
                  <th className="text-left px-3.5 py-2.5 text-zinc-600 font-medium text-[11px] uppercase tracking-wider w-32">Rate</th>
                </tr>
              </thead>
              <tbody>
                {actionNames.map((action, i) => {
                  const breakdown = data.by_action[action];
                  const actionRate = breakdown.total > 0
                    ? Math.round((breakdown.success / breakdown.total) * 100)
                    : 0;
                  return (
                    <tr
                      key={action}
                      className={`border-b border-white/[0.03] ${i % 2 === 1 ? "bg-white/[0.01]" : ""}`}
                    >
                      <td className="px-3.5 py-2.5 font-mono text-zinc-300">{action}</td>
                      <td className="px-3.5 py-2.5 text-right font-mono text-zinc-400">{breakdown.total}</td>
                      <td className="px-3.5 py-2.5 text-right font-mono text-emerald-400">{breakdown.success}</td>
                      <td className="px-3.5 py-2.5 text-right font-mono text-rose-400">{breakdown.error}</td>
                      <td className="px-3.5 py-2.5">
                        <div className="flex items-center gap-2">
                          <div className="flex-1 h-1.5 bg-white/[0.04] rounded-full overflow-hidden">
                            <div
                              className={`h-full rounded-full ${rateBgColor(actionRate)}`}
                              style={{ width: `${actionRate}%` }}
                            />
                          </div>
                          <span className={`text-[11px] font-mono ${rateColor(actionRate)}`}>
                            {actionRate}%
                          </span>
                        </div>
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        </div>
      )}

      {/* Failed Intents */}
      <div>
        <h2 className="text-[15px] font-semibold text-zinc-200 mb-3 tracking-tight">Failed Intents</h2>
        {!data || data.failed_intents.length === 0 ? (
          <div className="bg-[#0a0a0f] border border-white/[0.06] rounded-lg p-6 text-center">
            <p className="text-[13px] text-zinc-500">No failed intents recorded.</p>
          </div>
        ) : (
          <div className="bg-[#0a0a0f] border border-white/[0.06] rounded-lg overflow-hidden max-h-96 overflow-y-auto">
            {data.failed_intents.map((intent, i) => {
              const isUnmet = intent.entity_type === "";
              const ts = new Date(intent.timestamp);
              const timeStr = ts.toLocaleTimeString();
              return (
                <div
                  key={`${intent.timestamp}-${i}`}
                  className="flex items-center gap-3 px-3.5 py-2.5 border-b border-white/[0.03] last:border-b-0"
                >
                  <span className="text-[11px] text-zinc-600 font-mono flex-shrink-0 w-20">
                    {timeStr}
                  </span>
                  <span className="font-mono text-[11px] text-[#6c8cff] flex-shrink-0">
                    {intent.action}
                  </span>
                  {isUnmet ? (
                    <span className="text-[10px] font-medium bg-amber-500/15 text-amber-400 px-1.5 py-0.5 rounded flex-shrink-0">
                      unmet
                    </span>
                  ) : (
                    <span className="text-[10px] font-mono text-zinc-600 flex-shrink-0">
                      {intent.entity_type}
                    </span>
                  )}
                  {intent.entity_id && (
                    <span className="text-[10px] font-mono text-zinc-700 flex-shrink-0">
                      {intent.entity_id}
                    </span>
                  )}
                  <span className="text-[11px] text-rose-400 truncate ml-auto">
                    {intent.error}
                  </span>
                </div>
              );
            })}
          </div>
        )}
      </div>
    </div>
  );
}
