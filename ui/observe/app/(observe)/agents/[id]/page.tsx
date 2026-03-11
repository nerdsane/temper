"use client";

import { useState, useMemo } from "react";
import { useParams } from "next/navigation";
import Link from "next/link";
import { fetchAgentHistory } from "@/lib/api";
import { usePolling, useRelativeTime } from "@/lib/hooks";
import type { AgentHistoryResponse } from "@/lib/types";
import ErrorDisplay from "@/components/ErrorDisplay";
import StatCard from "@/components/StatCard";

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
        <div className="h-4 bg-[var(--color-border)] rounded w-24 mb-3" />
        <div className="h-6 bg-[var(--color-border)] rounded w-48 mb-1.5" />
        <div className="h-3.5 bg-[var(--color-border)] rounded w-64 mb-6" />
        <div className="grid grid-cols-4 gap-3 mb-6">
          {[0, 1, 2, 3].map((i) => (
            <div key={i} className="glass rounded p-3.5">
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
      {/* Breadcrumb */}
      <div className="mb-4">
        <Link
          href="/agents"
          className="text-[13px] text-[var(--color-text-muted)] hover:text-[var(--color-text-secondary)] transition-colors"
        >
          Agents
        </Link>
        <span className="text-[var(--color-text-muted)] mx-1.5">/</span>
        <span className="text-[13px] text-[var(--color-text-secondary)] font-mono">{agentId}</span>
      </div>

      {/* Header */}
      <div className="flex items-center justify-between mb-6">
        <div>
          <h1 className="text-2xl text-[var(--color-text-primary)] tracking-tight font-serif font-mono">
            {agentId}
          </h1>
          <p className="text-sm text-[var(--color-text-muted)] mt-0.5">
            Action history and authorization events
          </p>
        </div>
        <div className="flex items-center gap-3">
          {entityTypes.length > 1 && (
            <select
              value={entityTypeFilter}
              onChange={(e) => setEntityTypeFilter(e.target.value)}
              className="bg-[var(--color-bg-surface)] text-[var(--color-text-secondary)] text-xs rounded-sm px-2 py-1.5 focus:outline-none"
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
            <span className="text-xs text-[var(--color-text-muted)]">
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
          color="text-[var(--color-accent-teal)]"
        />
        <StatCard
          label="Errors"
          value={stats.errors}
          color={stats.errors > 0 ? "text-[var(--color-accent-pink)]" : undefined}
        />
        <StatCard
          label="Denials"
          value={stats.denials}
          color={stats.denials > 0 ? "text-[var(--color-accent-pink)]" : undefined}
        />
      </div>

      {/* Timeline */}
      {data && data.history.length > 0 ? (
        <div>
          <h2 className="text-base font-semibold text-[var(--color-text-primary)] mb-3 tracking-tight">
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
                  className={`flex items-center gap-3 px-3.5 py-2.5 border-b border-[var(--color-border)] last:border-b-0 ${
                    isDenial
                      ? "bg-[var(--color-accent-pink-dim)]"
                      : isError
                        ? "bg-[var(--color-accent-pink-dim)]"
                        : ""
                  }`}
                >
                  {/* Status indicator */}
                  <div
                    className={`w-1.5 h-1.5 rounded-full flex-shrink-0 ${
                      isDenial
                        ? "bg-[var(--color-accent-pink)]"
                        : isError
                          ? "bg-[var(--color-accent-pink)]"
                          : "bg-[var(--color-accent-teal)]"
                    }`}
                  />

                  {/* Timestamp */}
                  <span className="text-[11px] text-[var(--color-text-muted)] font-mono flex-shrink-0 w-36">
                    {timeStr}
                  </span>

                  {/* Action */}
                  <span className="font-mono text-[11px] text-[var(--color-accent-teal)] flex-shrink-0 w-28">
                    {entry.action}
                  </span>

                  {/* Entity */}
                  <span className="text-[10px] font-mono text-[var(--color-text-secondary)] flex-shrink-0 w-24">
                    {entry.entity_type}
                  </span>
                  <span className="text-[10px] font-mono text-[var(--color-text-muted)] flex-shrink-0 w-20 truncate">
                    {entry.entity_id}
                  </span>

                  {/* State transition */}
                  {entry.from_status && entry.to_status && (
                    <span className="text-[10px] font-mono text-[var(--color-text-muted)] flex-shrink-0">
                      {entry.from_status}{" "}
                      <span className="text-[var(--color-text-muted)]">&rarr;</span>{" "}
                      {entry.to_status}
                    </span>
                  )}

                  {/* Badges */}
                  <div className="flex items-center gap-1.5 ml-auto flex-shrink-0">
                    {isDenial && (
                      <span className="text-[10px] font-medium bg-[var(--color-accent-pink-dim)] text-[var(--color-accent-pink)] px-1.5 py-0.5 rounded">
                        denied
                      </span>
                    )}
                    {isError && entry.error && (
                      <span className="text-[11px] text-[var(--color-accent-pink)] truncate max-w-48">
                        {entry.error}
                      </span>
                    )}
                    {isDenial && entry.denied_resource && (
                      <span className="text-[10px] font-mono text-[var(--color-text-muted)] truncate max-w-32">
                        {entry.denied_resource}
                      </span>
                    )}
                    {entry.tenant && entry.tenant !== "default" && (
                      <span className="text-[10px] text-[var(--color-text-muted)] font-mono">
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
          <p className="text-sm text-[var(--color-text-secondary)]">
            No action history recorded for this agent.
          </p>
        </div>
      )}
    </div>
  );
}
