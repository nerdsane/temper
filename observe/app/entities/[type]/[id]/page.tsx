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
      <div className="h-4 bg-gray-800/60 rounded w-64 mb-2" />
      <div className="h-7 bg-gray-800 rounded w-48 mb-6" />
      <div className="bg-gray-900 border border-gray-800 rounded-lg p-6 mb-8">
        <div className="grid grid-cols-3 gap-6">
          {[0, 1, 2].map((i) => (
            <div key={i}>
              <div className="h-4 bg-gray-800 rounded w-20 mb-2" />
              <div className="h-5 bg-gray-800 rounded w-28" />
            </div>
          ))}
        </div>
      </div>
      <div className="h-5 bg-gray-800 rounded w-36 mb-4" />
      <div className="space-y-3">
        {[0, 1, 2].map((i) => (
          <div key={i} className="h-20 bg-gray-900 border border-gray-800 rounded-lg" />
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
    <div>
      {/* Header */}
      <div className="mb-6">
        <div className="flex items-center gap-2 text-sm text-gray-500 mb-1">
          <Link href="/" className="hover:text-gray-300">Dashboard</Link>
          <span>/</span>
          <span className="text-gray-300">Entities</span>
          <span>/</span>
          <span className="text-gray-300">{entityType}</span>
          <span>/</span>
          <span className="text-gray-300">{entityId}</span>
        </div>
        <h1 className="text-2xl font-bold text-gray-100">
          {entityType}: {entityId}
        </h1>
      </div>

      {/* Current state card */}
      <div className="bg-gray-900 border border-gray-800 rounded-lg p-6 mb-8">
        <div className="grid grid-cols-3 gap-6">
          <div>
            <div className="text-sm text-gray-500 mb-1">Entity Type</div>
            <div className="font-mono text-gray-200">{history.entity_type}</div>
          </div>
          <div>
            <div className="text-sm text-gray-500 mb-1">Entity ID</div>
            <div className="font-mono text-gray-200">{history.entity_id}</div>
          </div>
          <div>
            <div className="text-sm text-gray-500 mb-1">Current State</div>
            <div className="inline-block">
              <span className="font-mono text-lg font-semibold text-green-400 bg-green-900/30 px-3 py-1 rounded border border-green-800">
                {history.current_state}
              </span>
            </div>
          </div>
        </div>
      </div>

      {/* Quick actions */}
      <div className="flex gap-3 mb-8">
        <Link
          href={`/specs/${entityType}`}
          className="px-4 py-2 bg-gray-800 hover:bg-gray-700 text-gray-300 text-sm rounded-md transition-colors border border-gray-700"
        >
          View Spec
        </Link>
        <Link
          href={`/verify/${entityType}`}
          className="px-4 py-2 bg-gray-800 hover:bg-gray-700 text-gray-300 text-sm rounded-md transition-colors border border-gray-700"
        >
          Verify Spec
        </Link>
      </div>

      {/* Event history */}
      <div>
        <h2 className="text-lg font-semibold text-gray-200 mb-4">
          Event History ({history.events.length} events)
        </h2>
        <EntityTimeline events={history.events} />
      </div>
    </div>
  );
}
