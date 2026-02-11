"use client";

import { useEffect, useState } from "react";
import { useParams } from "next/navigation";
import Link from "next/link";
import { fetchEntityHistory } from "@/lib/api";
import type { EntityHistory } from "@/lib/mock-data";
import EntityTimeline from "@/components/EntityTimeline";

export default function EntityInspector() {
  const params = useParams();
  const entityType = params.type as string;
  const entityId = params.id as string;
  const [history, setHistory] = useState<EntityHistory | null>(null);
  const [loading, setLoading] = useState(true);

  useEffect(() => {
    async function load() {
      const data = await fetchEntityHistory(entityType, entityId);
      setHistory(data);
      setLoading(false);
    }
    load();
  }, [entityType, entityId]);

  if (loading) {
    return (
      <div className="flex items-center justify-center h-64">
        <div className="text-gray-500 text-sm">Loading entity...</div>
      </div>
    );
  }

  if (!history) {
    return (
      <div className="text-center py-12">
        <div className="text-gray-500 mb-4">
          No history found for {entityType}/{entityId}
        </div>
        <Link href="/" className="text-blue-400 hover:text-blue-300 text-sm">
          Back to Dashboard
        </Link>
      </div>
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
