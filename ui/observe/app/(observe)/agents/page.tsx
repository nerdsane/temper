"use client";

import { useState, useCallback, useMemo } from "react";
import { useRouter } from "next/navigation";
import { fetchAgents } from "@/lib/api";
import { usePolling, useRelativeTime } from "@/lib/hooks";
import type { AgentsResponse } from "@/lib/types";
import ErrorDisplay from "@/components/ErrorDisplay";
import StatCard from "@/components/StatCard";

function rateBgClass(rate: number): string {
  if (rate >= 80) return "bg-[var(--color-accent-teal-dim)] text-[var(--color-accent-teal)]";
  if (rate >= 50) return "bg-[var(--color-accent-pink-dim)] text-[var(--color-accent-pink)]";
  return "bg-[var(--color-accent-pink-dim)] text-[var(--color-accent-pink)]";
}

export default function AgentsPage() {
  const router = useRouter();
  const [initialError, setInitialError] = useState<string | null>(null);

  const loadInitial = useCallback(async () => {
    setInitialError(null);
    try {
      await fetchAgents();
    } catch (err) {
      setInitialError(
        err instanceof Error ? err.message : "Failed to load agents",
      );
    }
  }, []);

  const agentsPoll = usePolling<AgentsResponse>({
    fetcher: () => fetchAgents(),
    interval: 5000,
    enabled: !initialError,
  });

  const data = agentsPoll.data;
  const lastUpdated = useRelativeTime(agentsPoll.lastUpdated);

  const totalDenials = useMemo(() => {
    if (!data) return 0;
    return data.agents.reduce((sum, a) => sum + a.denial_count, 0);
  }, [data]);

  const totalErrors = useMemo(() => {
    if (!data) return 0;
    return data.agents.reduce((sum, a) => sum + a.error_count, 0);
  }, [data]);

  if (initialError) {
    return (
      <ErrorDisplay
        title="Cannot load agents"
        message={initialError}
        retry={loadInitial}
      />
    );
  }

  if (agentsPoll.loading && !data) {
    return (
      <div className="animate-pulse">
        <div className="h-6 bg-[var(--color-border)] rounded w-36 mb-1.5" />
        <div className="h-3.5 bg-[var(--color-border)] rounded w-64 mb-6" />
        <div className="grid grid-cols-4 gap-3 mb-6">
          {[0, 1, 2, 3].map((i) => (
            <div key={i} className="glass rounded-[2px] p-4">
              <div className="h-3 bg-[var(--color-border)] rounded w-20 mb-2" />
              <div className="h-8 bg-[var(--color-border)] rounded w-10" />
            </div>
          ))}
        </div>
      </div>
    );
  }

  return (
    <div className="animate-fade-in">
      {/* Header */}
      <div className="flex items-center justify-between mb-6">
        <div>
          <h1 className="text-2xl text-[var(--color-text-primary)] tracking-tight font-serif">
            Agents
          </h1>
          <p className="text-sm text-[var(--color-text-muted)] mt-0.5">
            Agent activity, success rates, and authorization denials
          </p>
        </div>
        <div className="flex items-center gap-3">
          {lastUpdated && (
            <span className="text-xs text-[var(--color-text-muted)]">
              Updated {lastUpdated}
            </span>
          )}
        </div>
      </div>

      {/* Stats row */}
      <div className="grid grid-cols-4 gap-3 mb-6">
        <StatCard label="Total Agents" value={data?.total ?? 0} />
        <StatCard
          label="Total Denials"
          value={totalDenials}
          color={totalDenials > 0 ? "text-[var(--color-accent-pink)]" : undefined}
        />
        <StatCard
          label="Total Errors"
          value={totalErrors}
          color={totalErrors > 0 ? "text-[var(--color-accent-pink)]" : undefined}
        />
        <StatCard
          label="Active Agents"
          value={
            data?.agents.filter((a) => a.last_active_at !== null).length ?? 0
          }
          color="text-[var(--color-accent-teal)]"
        />
      </div>

      {/* Agent Table */}
      {data && data.agents.length > 0 ? (
        <div className="glass rounded overflow-hidden">
          <table className="w-full text-[13px]">
            <thead className="sticky top-0 bg-[color-mix(in_srgb,var(--color-bg-surface)_90%,transparent)] backdrop-blur-sm z-10">
              <tr className="border-b border-[var(--color-border)]">
                <th className="text-left px-3.5 py-2.5 text-[var(--color-text-muted)] font-medium text-xs uppercase tracking-wider">
                  Agent ID
                </th>
                <th className="text-right px-3.5 py-2.5 text-[var(--color-text-muted)] font-medium text-xs uppercase tracking-wider">
                  Total
                </th>
                <th className="text-right px-3.5 py-2.5 text-[var(--color-text-muted)] font-medium text-xs uppercase tracking-wider">
                  Success
                </th>
                <th className="text-right px-3.5 py-2.5 text-[var(--color-text-muted)] font-medium text-xs uppercase tracking-wider">
                  Errors
                </th>
                <th className="text-right px-3.5 py-2.5 text-[var(--color-text-muted)] font-medium text-xs uppercase tracking-wider">
                  Denials
                </th>
                <th className="text-left px-3.5 py-2.5 text-[var(--color-text-muted)] font-medium text-xs uppercase tracking-wider">
                  Rate
                </th>
                <th className="text-left px-3.5 py-2.5 text-[var(--color-text-muted)] font-medium text-xs uppercase tracking-wider">
                  Entity Types
                </th>
                <th className="text-right px-3.5 py-2.5 text-[var(--color-text-muted)] font-medium text-xs uppercase tracking-wider">
                  Last Active
                </th>
              </tr>
            </thead>
            <tbody>
              {data.agents.map((agent, i) => {
                const rate = Math.round(agent.success_rate * 100);
                const lastActive = agent.last_active_at
                  ? new Date(agent.last_active_at).toLocaleString()
                  : "--";
                return (
                  <tr
                    key={agent.agent_id}
                    onClick={() => router.push(`/agents/${encodeURIComponent(agent.agent_id)}`)}
                    className={`border-b border-[var(--color-border)] hover:bg-[var(--color-bg-elevated)] transition-colors cursor-pointer ${i % 2 === 1 ? "bg-[var(--color-bg-elevated)]" : ""}`}
                  >
                    <td className="px-3.5 py-2.5">
                      <span className="font-mono text-[var(--color-text-primary)]">
                        {agent.agent_id}
                      </span>
                    </td>
                    <td className="px-3.5 py-2.5 text-right font-mono text-[var(--color-text-secondary)]">
                      {agent.total_actions}
                    </td>
                    <td className="px-3.5 py-2.5 text-right font-mono text-[var(--color-accent-teal)]">
                      {agent.success_count}
                    </td>
                    <td className="px-3.5 py-2.5 text-right font-mono text-[var(--color-accent-pink)]">
                      {agent.error_count}
                    </td>
                    <td className="px-3.5 py-2.5 text-right font-mono text-[var(--color-accent-pink)]">
                      {agent.denial_count}
                    </td>
                    <td className="px-3.5 py-2.5">
                      <span
                        className={`text-xs font-mono px-2 py-0.5 rounded-full ${rateBgClass(rate)}`}
                      >
                        {rate}%
                      </span>
                    </td>
                    <td className="px-3.5 py-2.5">
                      <div className="flex flex-wrap gap-1">
                        {agent.entity_types.map((et) => (
                          <span
                            key={et}
                            className="text-[10px] font-mono bg-[var(--color-bg-elevated)] text-[var(--color-text-secondary)] px-1.5 py-0.5 rounded-sm"
                          >
                            {et}
                          </span>
                        ))}
                      </div>
                    </td>
                    <td className="px-3.5 py-2.5 text-right font-mono text-[var(--color-text-muted)] text-[11px]">
                      {lastActive}
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      ) : (
        <div className="glass rounded-[2px] p-6 text-center">
          <p className="text-sm text-[var(--color-text-secondary)]">
            No agent activity recorded yet.
          </p>
        </div>
      )}
    </div>
  );
}
