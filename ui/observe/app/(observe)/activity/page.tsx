"use client";

import { useEffect, useState, useMemo, useCallback, useRef } from "react";
import Link from "next/link";
import { fetchTrajectories, fetchEntities, subscribeEntityEvents } from "@/lib/api";
import { usePolling, useRelativeTime } from "@/lib/hooks";
import type { TrajectoryResponse, EntityStateChange, EntitySummary } from "@/lib/types";
import ErrorDisplay from "@/components/ErrorDisplay";
import StatCard from "@/components/StatCard";
import StatusBadge from "@/components/StatusBadge";
import EntityDetailPanel from "@/components/EntityDetailPanel";
import { rateColor, rateBgColor } from "@/lib/utils";

export default function ActivityPage() {
  const [initialLoading, setInitialLoading] = useState(true);
  const [initialError, setInitialError] = useState<string | null>(null);
  const [liveEvents, setLiveEvents] = useState<EntityStateChange[]>([]);
  const [filterType, setFilterType] = useState<string>("all");
  const [failedPage, setFailedPage] = useState(1);
  const FAILED_PER_PAGE = 10;
  const feedRef = useRef<HTMLDivElement>(null);
  const prevEntityIdsRef = useRef<Set<string>>(new Set());
  const [newEntityKeys, setNewEntityKeys] = useState<Set<string>>(new Set());
  const prevFailedCountRef = useRef<number>(0);
  const [failedFlash, setFailedFlash] = useState("");
  const [newFailedCount, setNewFailedCount] = useState(0);

  // Entity detail panel state
  const [selectedEntity, setSelectedEntity] = useState<{ type: string; id: string } | null>(null);

  // Entity table state
  const [searchQuery, setSearchQuery] = useState("");
  const [entityTypeFilter, setEntityTypeFilter] = useState<string>("all");
  const [entityStateFilter, setEntityStateFilter] = useState<string>("all");

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

  // Live event feed
  useEffect(() => {
    if (initialLoading || initialError) return;
    const cleanup = subscribeEntityEvents((event) => {
      setLiveEvents((prev) => [...prev.slice(-199), event]);
      trajectoryPoll.refresh();
    });
    return cleanup;
  }, [initialLoading, initialError]); // eslint-disable-line react-hooks/exhaustive-deps

  // Auto-scroll live feed
  useEffect(() => {
    if (feedRef.current) {
      feedRef.current.scrollTop = feedRef.current.scrollHeight;
    }
  }, [liveEvents.length]);

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

  // Entity polling
  const entityPoll = usePolling<EntitySummary[]>({
    fetcher: fetchEntities,
    interval: 5000,
    enabled: !initialLoading && !initialError,
  });

  const entities = useMemo(() => entityPoll.data ?? [], [entityPoll.data]);

  // Derive unique entity types/states for filters
  const allEntityTypes = useMemo(() => {
    const set = new Set<string>();
    for (const e of entities) set.add(e.entity_type);
    return Array.from(set).sort();
  }, [entities]);

  const allEntityStates = useMemo(() => {
    const set = new Set<string>();
    for (const e of entities) {
      if (e.current_state) set.add(e.current_state);
      if (e.actor_status) set.add(e.actor_status);
    }
    return Array.from(set).sort();
  }, [entities]);

  // Track new entity rows for highlight
  useEffect(() => {
    const currentKeys = new Set(entities.map((e) => `${e.entity_type}-${e.entity_id}`));
    if (prevEntityIdsRef.current.size > 0) {
      const added = new Set<string>();
      for (const k of currentKeys) {
        if (!prevEntityIdsRef.current.has(k)) added.add(k);
      }
      if (added.size > 0) setNewEntityKeys(added);
    }
    prevEntityIdsRef.current = currentKeys;
  }, [entities]);

  // Track failed intent count for flash + per-item animation
  useEffect(() => {
    const count = data?.failed_intents?.length ?? 0;
    if (count > prevFailedCountRef.current && prevFailedCountRef.current > 0) {
      setFailedFlash("animate-flash-pink");
      setNewFailedCount(count - prevFailedCountRef.current);
    }
    prevFailedCountRef.current = count;
  }, [data?.failed_intents?.length]);

  const filteredEntities = useMemo(() => {
    let result = entities;
    if (entityTypeFilter !== "all") {
      result = result.filter((e) => e.entity_type === entityTypeFilter);
    }
    if (entityStateFilter !== "all") {
      result = result.filter(
        (e) => e.current_state === entityStateFilter || e.actor_status === entityStateFilter,
      );
    }
    if (searchQuery.trim()) {
      const q = searchQuery.toLowerCase();
      result = result.filter(
        (e) =>
          e.entity_type.toLowerCase().includes(q) ||
          e.entity_id.toLowerCase().includes(q),
      );
    }
    return result;
  }, [entities, entityTypeFilter, entityStateFilter, searchQuery]);

  if (initialLoading) {
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

  if (initialError) {
    return <ErrorDisplay title="Cannot load activity" message={initialError} retry={loadInitial} />;
  }

  const successRate = data ? Math.round(data.success_rate * 100) : 0;

  return (
    <div className="animate-fade-in">
      {/* Header */}
      <div className="flex items-center justify-between mb-6">
        <div>
          <h1 className="text-2xl text-[var(--color-text-primary)] tracking-tight font-serif">Activity</h1>
          <p className="text-sm text-[var(--color-text-muted)] mt-0.5">
            Trajectory stats, action breakdown, and failed intents
          </p>
        </div>
        <div className="flex items-center gap-3">
          {liveEvents.length > 0 && (
            <div className="flex items-center gap-1.5">
              <div className="w-1.5 h-1.5 bg-[var(--color-accent-teal)] rounded-full animate-pulse" />
              <span className="text-xs text-[var(--color-text-secondary)] font-mono">{liveEvents.length} live</span>
            </div>
          )}
          {entityTypes.length > 0 && (
            <select
              value={filterType}
              onChange={(e) => setFilterType(e.target.value)}
              className="bg-[var(--color-bg-surface)] text-[var(--color-text-secondary)] text-xs rounded-sm px-2 py-1.5 focus:outline-none"
            >
              <option value="all">All types</option>
              {entityTypes.map((t) => (
                <option key={t} value={t}>{t}</option>
              ))}
            </select>
          )}
          {lastUpdated && (
            <span className="text-xs text-[var(--color-text-muted)]">Updated {lastUpdated}</span>
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
        <StatCard label="Errors" value={data?.error_count ?? 0} color={data && data.error_count > 0 ? "text-[var(--color-accent-pink)]" : undefined} />
        <StatCard label="Actions Tracked" value={actionNames.length} />
      </div>

      {/* Live Activity Feed */}
      {liveEvents.length > 0 && (
        <div className="mb-6">
          <div className="flex items-center justify-between mb-3">
            <div className="flex items-center gap-2">
              <div className="w-1.5 h-1.5 bg-[var(--color-accent-teal)] rounded-full animate-pulse" />
              <h2 className="text-base font-semibold text-[var(--color-text-primary)] tracking-tight">Live Activity</h2>
              <span className="text-[10px] font-mono text-[var(--color-text-muted)]">{liveEvents.length}</span>
            </div>
            <button
              onClick={() => setLiveEvents([])}
              className="text-[11px] text-[var(--color-text-muted)] hover:text-[var(--color-text-secondary)] transition-colors"
            >
              Clear
            </button>
          </div>
          <div
            ref={feedRef}
            className="glass rounded overflow-hidden max-h-64 overflow-y-auto"
          >
            {liveEvents.map((event, i) => (
              <div
                key={i}
                className="flex items-center gap-3 px-3.5 py-2 border-b border-[var(--color-border)] last:border-b-0 animate-slide-in"
              >
                <div className="w-1.5 h-1.5 rounded-full bg-[var(--color-accent-teal)] flex-shrink-0" />
                <span className="font-mono text-[11px] text-[var(--color-text-secondary)] flex-shrink-0">
                  {event.entity_type}
                </span>
                <span className="font-mono text-[11px] text-[var(--color-text-muted)] flex-shrink-0">
                  {event.entity_id}
                </span>
                <span className="text-[11px] text-[var(--color-accent-teal)] font-mono flex-shrink-0">
                  {event.action}
                </span>
                <span className="text-[var(--color-text-muted)] text-[11px]">&rarr;</span>
                <span className="font-mono text-[11px] text-[var(--color-accent-teal)] flex-shrink-0">
                  {event.status}
                </span>
                {event.tenant && event.tenant !== "default" && (
                  <span className="text-[10px] text-[var(--color-text-muted)] font-mono ml-auto flex-shrink-0">
                    {event.tenant}
                  </span>
                )}
              </div>
            ))}
          </div>
        </div>
      )}

      {/* Action Breakdown */}
      {data && actionNames.length > 0 && (
        <div className="mb-6">
          <h2 className="text-base font-semibold text-[var(--color-text-primary)] mb-3 tracking-tight">Action Breakdown</h2>
          <div className="glass rounded overflow-hidden max-h-80 overflow-y-auto">
            <table className="w-full text-[13px]">
              <thead className="sticky top-0 bg-[color-mix(in_srgb,var(--color-bg-surface)_90%,transparent)] backdrop-blur-sm z-10">
                <tr className="border-b border-[var(--color-border)]">
                  <th className="text-left px-3.5 py-2.5 text-[var(--color-text-muted)] font-medium text-xs uppercase tracking-wider">Action</th>
                  <th className="text-right px-3.5 py-2.5 text-[var(--color-text-muted)] font-medium text-xs uppercase tracking-wider">Total</th>
                  <th className="text-right px-3.5 py-2.5 text-[var(--color-text-muted)] font-medium text-xs uppercase tracking-wider">Success</th>
                  <th className="text-right px-3.5 py-2.5 text-[var(--color-text-muted)] font-medium text-xs uppercase tracking-wider">Errors</th>
                  <th className="text-left px-3.5 py-2.5 text-[var(--color-text-muted)] font-medium text-xs uppercase tracking-wider w-32">Rate</th>
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
                      className={`border-b border-[var(--color-border)] ${i % 2 === 1 ? "bg-[var(--color-bg-elevated)]" : ""}`}
                    >
                      <td className="px-3.5 py-2.5 font-mono text-[var(--color-text-secondary)]">{action}</td>
                      <td className="px-3.5 py-2.5 text-right font-mono text-[var(--color-text-secondary)]">{breakdown.total}</td>
                      <td className="px-3.5 py-2.5 text-right font-mono text-[var(--color-accent-teal)]">{breakdown.success}</td>
                      <td className="px-3.5 py-2.5 text-right font-mono text-[var(--color-accent-pink)]">{breakdown.error}</td>
                      <td className="px-3.5 py-2.5">
                        <div className="flex items-center gap-2">
                          <div className="flex-1 h-1.5 bg-[var(--color-bg-elevated)] rounded-full overflow-hidden">
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

      {/* Active Entities */}
      {entities.length > 0 && (
        <div className="mb-6">
          <div className="flex items-center justify-between mb-3">
            <h2 className="text-base font-semibold text-[var(--color-text-primary)] tracking-tight">
              Active Entities
              <span className="text-[var(--color-text-muted)] font-normal text-[13px] ml-2">{filteredEntities.length}</span>
            </h2>
            <div className="flex items-center gap-2">
              <input
                type="text"
                placeholder="Search entities..."
                value={searchQuery}
                onChange={(e) => setSearchQuery(e.target.value)}
                className="bg-[var(--color-bg-surface)] text-[var(--color-text-secondary)] text-xs rounded-sm px-2.5 py-1.5 w-40 placeholder:text-[var(--color-text-muted)] focus:outline-none focus:ring-1 focus:ring-[var(--color-accent-teal)]"
              />
              {allEntityTypes.length > 1 && (
                <select
                  value={entityTypeFilter}
                  onChange={(e) => setEntityTypeFilter(e.target.value)}
                  className="bg-[var(--color-bg-surface)] text-[var(--color-text-secondary)] text-xs rounded-sm px-2 py-1.5 focus:outline-none"
                >
                  <option value="all">All types</option>
                  {allEntityTypes.map((t) => (
                    <option key={t} value={t}>{t}</option>
                  ))}
                </select>
              )}
              {allEntityStates.length > 1 && (
                <select
                  value={entityStateFilter}
                  onChange={(e) => setEntityStateFilter(e.target.value)}
                  className="bg-[var(--color-bg-surface)] text-[var(--color-text-secondary)] text-xs rounded-sm px-2 py-1.5 focus:outline-none"
                >
                  <option value="all">All states</option>
                  {allEntityStates.map((s) => (
                    <option key={s} value={s}>{s}</option>
                  ))}
                </select>
              )}
              {(searchQuery || entityTypeFilter !== "all" || entityStateFilter !== "all") && (
                <button
                  onClick={() => { setSearchQuery(""); setEntityTypeFilter("all"); setEntityStateFilter("all"); }}
                  className="text-[11px] text-[var(--color-text-muted)] hover:text-[var(--color-text-secondary)] transition-colors"
                >
                  Clear
                </button>
              )}
            </div>
          </div>
          <div className="glass rounded overflow-hidden max-h-72 overflow-y-auto">
            <table className="w-full text-[13px]">
              <thead className="sticky top-0 bg-[color-mix(in_srgb,var(--color-bg-surface)_90%,transparent)] backdrop-blur-sm z-10">
                <tr className="border-b border-[var(--color-border)]">
                  <th className="text-left px-3.5 py-2.5 text-[var(--color-text-muted)] font-medium text-xs uppercase tracking-wider">Type</th>
                  <th className="text-left px-3.5 py-2.5 text-[var(--color-text-muted)] font-medium text-xs uppercase tracking-wider">ID</th>
                  <th className="text-left px-3.5 py-2.5 text-[var(--color-text-muted)] font-medium text-xs uppercase tracking-wider">Status</th>
                  <th className="text-right px-3.5 py-2.5 text-[var(--color-text-muted)] font-medium text-xs uppercase tracking-wider" />
                </tr>
              </thead>
              <tbody>
                {filteredEntities.map((entity) => {
                  const eKey = `${entity.entity_type}-${entity.entity_id}`;
                  const isNew = newEntityKeys.has(eKey);
                  return (
                  <tr
                    key={eKey}
                    className={`border-b border-[var(--color-border)] hover:bg-[var(--color-bg-elevated)] transition-colors cursor-pointer ${isNew ? "animate-highlight-new" : ""}`}
                    onClick={() => setSelectedEntity({ type: entity.entity_type, id: entity.entity_id })}
                    onAnimationEnd={() => { if (isNew) setNewEntityKeys((prev) => { const next = new Set(prev); next.delete(eKey); return next; }); }}
                  >
                    <td className="px-3.5 py-2.5 font-mono text-[var(--color-text-secondary)]">{entity.entity_type}</td>
                    <td className="px-3.5 py-2.5 font-mono text-[var(--color-text-secondary)]">{entity.entity_id}</td>
                    <td className="px-3.5 py-2.5">
                      <StatusBadge status={entity.current_state ?? entity.actor_status} />
                    </td>
                    <td className="px-3.5 py-2.5 text-right">
                      <Link
                        href={`/entities/${entity.entity_type}/${entity.entity_id}`}
                        className="text-[11px] text-[var(--color-accent-teal)] hover:text-[var(--color-accent-teal)] transition-colors"
                        onClick={(e) => e.stopPropagation()}
                      >
                        Inspect
                      </Link>
                    </td>
                  </tr>
                  );
                })}
              </tbody>
            </table>
            {filteredEntities.length === 0 && (
              <div className="px-3.5 py-6 text-center text-[13px] text-[var(--color-text-muted)]">
                No entities match the current filters.
              </div>
            )}
          </div>
        </div>
      )}

      {/* Failed Intents */}
      <div>
        <h2
          className={`text-base font-semibold text-[var(--color-text-primary)] mb-3 tracking-tight rounded px-1 -mx-1 ${failedFlash}`}
          onAnimationEnd={() => setFailedFlash("")}
        >
          Failed Intents
        </h2>
        {!data || data.failed_intents.length === 0 ? (
          <div className="glass rounded-[2px] p-6 text-center">
            <p className="text-sm text-[var(--color-text-secondary)]">No failed intents recorded.</p>
          </div>
        ) : (
          <div className="glass rounded overflow-hidden">
            {(() => {
              const totalFailed = data.failed_intents.length;
              const totalPages = Math.ceil(totalFailed / FAILED_PER_PAGE);
              const reversed = [...data.failed_intents].reverse();
              const paginatedIntents = reversed.slice(
                (failedPage - 1) * FAILED_PER_PAGE,
                failedPage * FAILED_PER_PAGE,
              );
              return (
                <>
                  <div className="max-h-96 overflow-y-auto">
                    {paginatedIntents.map((intent, i) => {
                      const isUnmet = intent.entity_type === "";
                      const ts = new Date(intent.timestamp);
                      const timeStr = ts.toLocaleTimeString();
                      // Animate newly arrived items (first page only, newest items)
                      const isNewItem = failedPage === 1 && i < newFailedCount;
                      return (
                        <div
                          key={`${intent.timestamp}-${i}`}
                          className={`flex items-center gap-3 px-3.5 py-2.5 border-b border-[var(--color-border)] last:border-b-0 ${isNewItem ? "animate-item-slide-in" : ""}`}
                          onAnimationEnd={() => { if (isNewItem && i === newFailedCount - 1) setNewFailedCount(0); }}
                        >
                          <span className="text-[11px] text-[var(--color-text-muted)] font-mono flex-shrink-0 w-20">
                            {timeStr}
                          </span>
                          <span className="font-mono text-[11px] text-[var(--color-accent-teal)] flex-shrink-0">
                            {intent.action}
                          </span>
                          {isUnmet ? (
                            <span className="text-[10px] font-medium bg-[var(--color-accent-pink-dim)] text-[var(--color-accent-pink)] px-1.5 py-0.5 rounded flex-shrink-0" style={{ boxShadow: "0 0 6px 1px rgba(234, 179, 8, 0.15)" }}>
                              unmet
                            </span>
                          ) : (
                            <span className="text-[10px] font-mono text-[var(--color-text-muted)] flex-shrink-0">
                              {intent.entity_type}
                            </span>
                          )}
                          {intent.entity_id && (
                            <span className="text-[10px] font-mono text-[var(--color-text-muted)] flex-shrink-0">
                              {intent.entity_id}
                            </span>
                          )}
                          <span className="text-[11px] text-[var(--color-accent-pink)] truncate ml-auto">
                            {intent.error}
                          </span>
                        </div>
                      );
                    })}
                  </div>
                  {totalFailed > FAILED_PER_PAGE && (
                    <div className="flex items-center justify-between px-4 py-3 border-t border-[var(--color-border)]">
                      <span className="text-xs text-[var(--color-text-secondary)]">
                        Showing {(failedPage - 1) * FAILED_PER_PAGE + 1}-{Math.min(failedPage * FAILED_PER_PAGE, totalFailed)} of {totalFailed}
                      </span>
                      <div className="flex gap-2">
                        <button
                          onClick={() => setFailedPage((p) => Math.max(1, p - 1))}
                          disabled={failedPage === 1}
                          className="px-3 py-1 text-xs rounded-sm bg-[var(--color-bg-elevated)] text-[var(--color-text-secondary)] hover:bg-[var(--color-border-hover)] disabled:opacity-30 disabled:cursor-not-allowed transition-colors"
                        >
                          Prev
                        </button>
                        <button
                          onClick={() => setFailedPage((p) => Math.min(totalPages, p + 1))}
                          disabled={failedPage === totalPages}
                          className="px-3 py-1 text-xs rounded-sm bg-[var(--color-bg-elevated)] text-[var(--color-text-secondary)] hover:bg-[var(--color-border-hover)] disabled:opacity-30 disabled:cursor-not-allowed transition-colors"
                        >
                          Next
                        </button>
                      </div>
                    </div>
                  )}
                </>
              );
            })()}
          </div>
        )}
      </div>

      {/* Entity Detail Panel */}
      {selectedEntity && (
        <EntityDetailPanel
          entityType={selectedEntity.type}
          entityId={selectedEntity.id}
          onClose={() => setSelectedEntity(null)}
        />
      )}
    </div>
  );
}
