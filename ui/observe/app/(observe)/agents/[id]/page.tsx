"use client";

import { useState, useMemo } from "react";
import { useParams } from "next/navigation";
import Link from "next/link";
import { fetchAgentHistory } from "@/lib/api";
import { usePolling, useRelativeTime } from "@/lib/hooks";
import type { AgentHistoryResponse } from "@/lib/types";
import ErrorDisplay from "@/components/ErrorDisplay";

function StatCard({
  label,
  value,
  color,
}: {
  label: string;
  value: string | number;
  color?: string;
}) {
  return (
    <div className="glass rounded p-3.5">
      <div className="text-[12px] text-zinc-600">{label}</div>
      <div
        className={`text-4xl font-bold font-mono mt-0.5 ${color ?? "text-zinc-100"}`}
      >
        {value}
      </div>
    </div>
  );
}

export default function AgentDetailPage() {
  const params = useParams();
  const agentId = decodeURIComponent(params.id as string);
  const [entityTypeFilter, setEntityTypeFilter] = useState<string>("all");

  const historyPoll = usePolling<AgentHistoryResponse>({
    fetcher: () =>
      fetchAgentHistory(agentId, {
        entity_type: entityTypeFilter !== "all" ? entityTypeFilter : undefined,
        limit: 200,
      }),
    interval: 5000,
    enabled: true,
  });

  const data = historyPoll.data;
  const lastUpdated = useRelativeTime(historyPoll.lastUpdated);

  const entityTypes = useMemo(() => {
    if (!data) return [];
    const set = new Set<string>();
    for (const entry of data.history) {
      if (entry.entity_type) set.add(entry.entity_type);
    }
    return Array.from(set).sort();
  }, [data]);

  const stats = useMemo(() => {
    if (!data) return { total: 0, success: 0, errors: 0, denials: 0 };
    let success = 0;
    let errors = 0;
    let denials = 0;
    for (const entry of data.history) {
      if (entry.authz_denied) denials++;
      else if (entry.success) success++;
      else errors++;
    }
    return { total: data.total, success, errors, denials };
  }, [data]);

  if (historyPoll.error && !data) {
    return (
      <ErrorDisplay
        title={`Cannot load agent ${agentId}`}
        message={historyPoll.error}
        retry={() => historyPoll.refresh()}
        backHref="/agents"
        backLabel="Back to Agents"
      />
    );
  }

  if (historyPoll.loading && !data) {
    return (
      <div className="animate-pulse">
        <div className="h-4 bg-zinc-800/40 rounded w-24 mb-3" />
        <div className="h-6 bg-zinc-800/60 rounded w-48 mb-1.5" />
        <div className="h-3.5 bg-zinc-800/40 rounded w-64 mb-6" />
        <div className="grid grid-cols-4 gap-3 mb-6">
          {[0, 1, 2, 3].map((i) => (
            <div key={i} className="glass rounded p-3.5">
              <div className="h-3 bg-zinc-800/50 rounded w-20 mb-2" />
              <div className="h-8 bg-zinc-800/50 rounded w-10" />
            </div>
          ))}
        </div>
      </div>
    );
  }

  return (
    <div className="animate-fade-in">
      {/* Breadcrumb */}
      <div className="mb-4">
        <Link
          href="/agents"
          className="text-[13px] text-zinc-600 hover:text-zinc-400 transition-colors"
        >
          Agents
        </Link>
        <span className="text-zinc-700 mx-1.5">/</span>
        <span className="text-[13px] text-zinc-300 font-mono">{agentId}</span>
      </div>

      {/* Header */}
      <div className="flex items-center justify-between mb-6">
        <div>
          <h1 className="text-2xl font-bold text-zinc-100 tracking-tight font-display font-mono">
            {agentId}
          </h1>
          <p className="text-sm text-zinc-600 mt-0.5">
            Action history and authorization events
          </p>
        </div>
        <div className="flex items-center gap-3">
          {entityTypes.length > 1 && (
            <select
              value={entityTypeFilter}
              onChange={(e) => setEntityTypeFilter(e.target.value)}
              className="bg-[#111115] text-zinc-400 text-xs rounded-sm px-2 py-1.5 focus:outline-none"
            >
              <option value="all">All types</option>
              {entityTypes.map((t) => (
                <option key={t} value={t}>
                  {t}
                </option>
              ))}
            </select>
          )}
          {lastUpdated && (
            <span className="text-xs text-zinc-600">
              Updated {lastUpdated}
            </span>
          )}
        </div>
      </div>

      {/* Stats row */}
      <div className="grid grid-cols-4 gap-3 mb-6">
        <StatCard label="Total Actions" value={stats.total} />
        <StatCard
          label="Successful"
          value={stats.success}
          color="text-teal-400"
        />
        <StatCard
          label="Errors"
          value={stats.errors}
          color={stats.errors > 0 ? "text-amber-400" : undefined}
        />
        <StatCard
          label="Denials"
          value={stats.denials}
          color={stats.denials > 0 ? "text-pink-400" : undefined}
        />
      </div>

      {/* Timeline */}
      {data && data.history.length > 0 ? (
        <div>
          <h2 className="text-base font-semibold text-zinc-200 mb-3 tracking-tight">
            Action Timeline
          </h2>
          <div className="glass rounded overflow-hidden max-h-[600px] overflow-y-auto">
            {data.history.map((entry, i) => {
              const ts = new Date(entry.timestamp);
              const timeStr = ts.toLocaleString();
              const isDenial = entry.authz_denied;
              const isError = !entry.success && !entry.authz_denied;

              return (
                <div
                  key={`${entry.timestamp}-${i}`}
                  className={`flex items-center gap-3 px-3.5 py-2.5 border-b border-white/[0.03] last:border-b-0 ${
                    isDenial
                      ? "bg-pink-500/[0.03]"
                      : isError
                        ? "bg-amber-500/[0.02]"
                        : ""
                  }`}
                >
                  {/* Status indicator */}
                  <div
                    className={`w-1.5 h-1.5 rounded-full flex-shrink-0 ${
                      isDenial
                        ? "bg-pink-400"
                        : isError
                          ? "bg-amber-400"
                          : "bg-teal-400"
                    }`}
                  />

                  {/* Timestamp */}
                  <span className="text-[11px] text-zinc-600 font-mono flex-shrink-0 w-36">
                    {timeStr}
                  </span>

                  {/* Action */}
                  <span className="font-mono text-[11px] text-teal-400 flex-shrink-0 w-28">
                    {entry.action}
                  </span>

                  {/* Entity */}
                  <span className="text-[10px] font-mono text-zinc-500 flex-shrink-0 w-24">
                    {entry.entity_type}
                  </span>
                  <span className="text-[10px] font-mono text-zinc-700 flex-shrink-0 w-20 truncate">
                    {entry.entity_id}
                  </span>

                  {/* State transition */}
                  {entry.from_status && entry.to_status && (
                    <span className="text-[10px] font-mono text-zinc-600 flex-shrink-0">
                      {entry.from_status}{" "}
                      <span className="text-zinc-700">&rarr;</span>{" "}
                      {entry.to_status}
                    </span>
                  )}

                  {/* Badges */}
                  <div className="flex items-center gap-1.5 ml-auto flex-shrink-0">
                    {isDenial && (
                      <span className="text-[10px] font-medium bg-pink-500/15 text-pink-400 px-1.5 py-0.5 rounded">
                        denied
                      </span>
                    )}
                    {isError && entry.error && (
                      <span className="text-[11px] text-amber-400 truncate max-w-48">
                        {entry.error}
                      </span>
                    )}
                    {isDenial && entry.denied_resource && (
                      <span className="text-[10px] font-mono text-zinc-600 truncate max-w-32">
                        {entry.denied_resource}
                      </span>
                    )}
                    {entry.tenant && entry.tenant !== "default" && (
                      <span className="text-[10px] text-zinc-700 font-mono">
                        {entry.tenant}
                      </span>
                    )}
                  </div>
                </div>
              );
            })}
          </div>
        </div>
      ) : (
        <div className="glass rounded p-6 text-center">
          <p className="text-sm text-zinc-500">
            No action history recorded for this agent.
          </p>
        </div>
      )}
    </div>
  );
}
