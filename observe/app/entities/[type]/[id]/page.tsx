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
      <div className="h-3.5 bg-zinc-800/40 rounded w-56 mb-1.5" />
      <div className="h-6 bg-zinc-800/60 rounded w-44 mb-5" />
      <div className="bg-[#111115] rounded-lg p-5 mb-6">
        <div className="grid grid-cols-3 gap-5">
          {[0, 1, 2].map((i) => (
            <div key={i}>
              <div className="h-3 bg-zinc-800/50 rounded w-16 mb-1.5" />
              <div className="h-4 bg-zinc-800/50 rounded w-24" />
            </div>
          ))}
        </div>
      </div>
      <div className="h-4 bg-zinc-800/50 rounded w-32 mb-3" />
      <div className="space-y-2.5">
        {[0, 1, 2].map((i) => (
          <div key={i} className="h-16 bg-[#111115] rounded-lg" />
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
        <div className="flex items-center gap-1.5 text-[12px] text-zinc-600 mb-1">
          <Link href="/" className="hover:text-zinc-400 transition-colors">Dashboard</Link>
          <span>/</span>
          <span className="text-zinc-400">Entities</span>
          <span>/</span>
          <span className="text-zinc-400">{entityType}</span>
          <span>/</span>
          <span className="text-zinc-400">{entityId}</span>
        </div>
        <h1 className="text-2xl font-bold text-zinc-100 tracking-tight font-display">
          {entityType}: {entityId}
        </h1>
      </div>

      {/* Current state card */}
      <div className="bg-[#111115] rounded-lg p-5 mb-6">
        <div className="grid grid-cols-3 gap-5">
          <div>
            <div className="text-[11px] text-zinc-600 mb-1 uppercase tracking-wider">Entity Type</div>
            <div className="font-mono text-[13px] text-zinc-200">{history.entity_type}</div>
          </div>
          <div>
            <div className="text-[11px] text-zinc-600 mb-1 uppercase tracking-wider">Entity ID</div>
            <div className="font-mono text-[13px] text-zinc-200">{history.entity_id}</div>
          </div>
          <div>
            <div className="text-[11px] text-zinc-600 mb-1 uppercase tracking-wider">Current State</div>
            <div className="inline-block">
              <span className="font-mono text-[14px] font-semibold text-teal-400 bg-teal-500/10 px-2.5 py-1 rounded-md">
                {history.current_state}
              </span>
            </div>
          </div>
        </div>
      </div>

      {/* Quick actions */}
      <div className="flex gap-2.5 mb-6">
        <Link
          href={`/specs/${entityType}`}
          className="px-3.5 py-1.5 bg-white/[0.04] hover:bg-white/[0.08] text-zinc-400 text-[13px] rounded-md transition-colors"
        >
          View Spec
        </Link>
        <Link
          href={`/verify/${entityType}`}
          className="px-3.5 py-1.5 bg-white/[0.04] hover:bg-white/[0.08] text-zinc-400 text-[13px] rounded-md transition-colors"
        >
          Verify Spec
        </Link>
      </div>

      {/* Event history */}
      <div>
        <h2 className="text-base font-semibold text-zinc-200 mb-3 tracking-tight">
          Event History ({history.events.length} events)
        </h2>
        <EntityTimeline events={history.events} />
      </div>
    </div>
  );
}
