"use client";

import { useEffect, useState, useCallback } from "react";
import { useParams } from "next/navigation";
import Link from "next/link";
import { fetchEntityHistory } from "@/lib/api";
import type { EntityHistory } from "@/lib/types";
import EntityTimeline from "@/components/EntityTimeline";
import ErrorDisplay from "@/components/ErrorDisplay";

function EntitySkeleton() {
  return (
    <div className="animate-pulse">
      <div className="h-3.5 bg-[var(--color-border)] rounded w-56 mb-1.5" />
      <div className="h-6 bg-[var(--color-border)] rounded w-44 mb-5" />
      <div className="glass rounded p-5 mb-6">
        <div className="grid grid-cols-3 gap-5">
          {[0, 1, 2].map((i) => (
            <div key={i}>
              <div className="h-3 bg-[var(--color-border)] rounded w-16 mb-1.5" />
              <div className="h-4 bg-[var(--color-border)] rounded w-24" />
            </div>
          ))}
        </div>
      </div>
      <div className="h-4 bg-[var(--color-border)] rounded w-32 mb-3" />
      <div className="space-y-2.5">
        {[0, 1, 2].map((i) => (
          <div key={i} className="h-16 glass rounded" />
        ))}
      </div>
    </div>
  );
}

export default function EntityInspector() {
  const params = useParams();
  const entityType = params.type as string;
  const entityId = params.id as string;
  const [history, setHistory] = useState<EntityHistory | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const data = await fetchEntityHistory(entityType, entityId);
      setHistory(data);
    } catch (err) {
      setError(
        err instanceof Error ? err.message : `Failed to load entity ${entityType}/${entityId}`,
      );
    } finally {
      setLoading(false);
    }
  }, [entityType, entityId]);

  useEffect(() => {
    load();
  }, [load]);

  if (loading) return <EntitySkeleton />;
  if (error) {
    return (
      <ErrorDisplay
        title={`Entity not found: ${entityType}/${entityId}`}
        message={error}
        retry={load}
      />
    );
  }
  if (!history) {
    return (
      <ErrorDisplay
        title="Entity not found"
        message={`No history found for ${entityType}/${entityId}.`}
      />
    );
  }

  return (
    <div className="animate-fade-in">
      {/* Header */}
      <div className="mb-5">
        <div className="flex items-center gap-1.5 text-[12px] text-[var(--color-text-muted)] mb-1">
          <Link href="/" className="hover:text-[var(--color-text-secondary)] transition-colors">Dashboard</Link>
          <span>/</span>
          <span className="text-[var(--color-text-secondary)]">Entities</span>
          <span>/</span>
          <span className="text-[var(--color-text-secondary)]">{entityType}</span>
          <span>/</span>
          <span className="text-[var(--color-text-secondary)]">{entityId}</span>
        </div>
        <h1 className="text-2xl text-[var(--color-text-primary)] tracking-tight font-serif">
          {entityType}: {entityId}
        </h1>
      </div>

      {/* Current state card */}
      <div className="glass rounded p-5 mb-6">
        <div className="grid grid-cols-3 gap-5">
          <div>
            <div className="text-[11px] text-[var(--color-text-muted)] mb-1 uppercase tracking-wider">Entity Type</div>
            <div className="font-mono text-[13px] text-[var(--color-text-primary)]">{history.entity_type}</div>
          </div>
          <div>
            <div className="text-[11px] text-[var(--color-text-muted)] mb-1 uppercase tracking-wider">Entity ID</div>
            <div className="font-mono text-[13px] text-[var(--color-text-primary)]">{history.entity_id}</div>
          </div>
          <div>
            <div className="text-[11px] text-[var(--color-text-muted)] mb-1 uppercase tracking-wider">Current State</div>
            <div className="inline-block">
              <span className="font-mono text-[14px] font-semibold text-[var(--color-accent-teal)] bg-[var(--color-accent-teal-dim)] px-2.5 py-1 rounded-sm">
                {history.current_state}
              </span>
            </div>
          </div>
        </div>
      </div>

      {/* Properties */}
      {(history.fields || history.counters || history.booleans || history.lists) && (
        <div className="mb-6">
          <h2 className="text-base font-semibold text-[var(--color-text-primary)] mb-3 tracking-tight">Properties</h2>
          <div className="glass rounded overflow-hidden">
            {/* Fields */}
            {history.fields && Object.keys(history.fields).length > 0 && (
              <div className="px-4 py-3 border-b border-[var(--color-border)]">
                <div className="text-[10px] text-[var(--color-text-muted)] uppercase tracking-widest mb-2">Fields</div>
                <div className="space-y-1.5">
                  {Object.entries(history.fields).map(([key, val]) => (
                    <div key={key} className="flex items-center justify-between">
                      <span className="text-[12px] text-[var(--color-text-secondary)]">{key}</span>
                      <span className="font-mono text-[12px] text-[var(--color-text-secondary)] truncate ml-4 max-w-[60%] text-right">
                        {typeof val === "object" ? JSON.stringify(val) : String(val ?? "—")}
                      </span>
                    </div>
                  ))}
                </div>
              </div>
            )}
            {/* Counters */}
            {history.counters && Object.keys(history.counters).length > 0 && (
              <div className="px-4 py-3 border-b border-[var(--color-border)]">
                <div className="text-[10px] text-[var(--color-text-muted)] uppercase tracking-widest mb-2">Counters</div>
                <div className="flex flex-wrap gap-3">
                  {Object.entries(history.counters).map(([key, val]) => (
                    <div key={key} className="bg-[var(--color-bg-elevated)] rounded-sm px-2.5 py-1.5">
                      <span className="text-[11px] text-[var(--color-text-secondary)] mr-2">{key}</span>
                      <span className="font-mono text-[13px] font-semibold text-[var(--color-text-primary)]">{val}</span>
                    </div>
                  ))}
                </div>
              </div>
            )}
            {/* Booleans */}
            {history.booleans && Object.keys(history.booleans).length > 0 && (
              <div className="px-4 py-3 border-b border-[var(--color-border)]">
                <div className="text-[10px] text-[var(--color-text-muted)] uppercase tracking-widest mb-2">Booleans</div>
                <div className="flex flex-wrap gap-3">
                  {Object.entries(history.booleans).map(([key, val]) => (
                    <div key={key} className="flex items-center gap-1.5">
                      <div className={`w-1.5 h-1.5 rounded-full ${val ? "bg-[var(--color-accent-teal)]" : "bg-[var(--color-text-muted)]"}`} />
                      <span className="text-[12px] text-[var(--color-text-secondary)]">{key}</span>
                    </div>
                  ))}
                </div>
              </div>
            )}
            {/* Lists */}
            {history.lists && Object.keys(history.lists).length > 0 && (
              <div className="px-4 py-3">
                <div className="text-[10px] text-[var(--color-text-muted)] uppercase tracking-widest mb-2">Lists</div>
                <div className="space-y-2">
                  {Object.entries(history.lists).map(([key, items]) => (
                    <div key={key}>
                      <span className="text-[11px] text-[var(--color-text-secondary)]">{key}</span>
                      <div className="flex flex-wrap gap-1.5 mt-1">
                        {items.length > 0 ? items.map((item, i) => (
                          <span key={i} className="font-mono text-[11px] text-[var(--color-text-secondary)] bg-[var(--color-bg-elevated)] px-1.5 py-0.5 rounded-sm">
                            {item}
                          </span>
                        )) : (
                          <span className="text-[11px] text-[var(--color-text-muted)]">empty</span>
                        )}
                      </div>
                    </div>
                  ))}
                </div>
              </div>
            )}
          </div>
        </div>
      )}

      {/* Quick actions */}
      <div className="flex gap-2.5 mb-6">
        <Link
          href={`/specs/${entityType}`}
          className="px-3.5 py-1.5 bg-[var(--color-bg-elevated)] hover:bg-[var(--color-border-hover)] text-[var(--color-text-secondary)] text-[13px] rounded-sm transition-colors"
        >
          View Spec
        </Link>
        <Link
          href={`/verify/${entityType}`}
          className="px-3.5 py-1.5 bg-[var(--color-bg-elevated)] hover:bg-[var(--color-border-hover)] text-[var(--color-text-secondary)] text-[13px] rounded-sm transition-colors"
        >
          Verify Spec
        </Link>
      </div>

      {/* Event history */}
      <div>
        <h2 className="text-base font-semibold text-[var(--color-text-primary)] mb-3 tracking-tight">
          Event History ({history.events.length} events)
        </h2>
        <EntityTimeline events={history.events} />
      </div>
    </div>
  );
}
