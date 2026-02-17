"use client";

import { useEffect, useState, useMemo, useCallback } from "react";
import { useParams } from "next/navigation";
import { fetchWorkflows, subscribeDesignTimeEvents } from "@/lib/api";
import { usePolling } from "@/lib/hooks";
import type { WorkflowsResponse, AppWorkflow, DesignTimeEvent } from "@/lib/types";
import WorkflowTimeline from "@/components/WorkflowTimeline";
import ErrorDisplay from "@/components/ErrorDisplay";
import Link from "next/link";

function DetailSkeleton() {
  return (
    <div className="animate-pulse">
      <div className="h-4 bg-gray-800/60 rounded w-24 mb-4" />
      <div className="h-7 bg-gray-800 rounded w-48 mb-2" />
      <div className="h-4 bg-gray-800/60 rounded w-72 mb-8" />
      <div className="space-y-6">
        {[0, 1, 2].map((i) => (
          <div key={i} className="bg-gray-900 border border-gray-800 rounded-lg p-5 h-64" />
        ))}
      </div>
    </div>
  );
}

function StatusBanner({ status }: { status: string }) {
  const configs: Record<string, { bg: string; border: string; text: string; label: string }> = {
    loading: { bg: "bg-blue-950/30", border: "border-blue-800", text: "text-blue-400", label: "Loading specs..." },
    verifying: { bg: "bg-yellow-950/30", border: "border-yellow-800", text: "text-yellow-400", label: "Verification in progress" },
    completed: { bg: "bg-green-950/30", border: "border-green-800", text: "text-green-400", label: "All entities verified and deployed" },
    failed: { bg: "bg-red-950/30", border: "border-red-800", text: "text-red-400", label: "Some entities failed verification" },
  };
  const c = configs[status] ?? configs.loading;
  const isActive = status === "loading" || status === "verifying";

  return (
    <div className={`${c.bg} border ${c.border} rounded-lg p-4 mb-6 flex items-center gap-3`}>
      {isActive && <div className="w-2 h-2 bg-yellow-400 rounded-full animate-pulse" />}
      {!isActive && (
        <span className={`text-lg ${c.text}`}>
          {status === "completed" ? "\u2713" : "\u2717"}
        </span>
      )}
      <span className={`text-sm font-medium ${c.text}`}>{c.label}</span>
    </div>
  );
}

export default function WorkflowDetailPage() {
  const params = useParams();
  const tenant = params.tenant as string;
  const [initialLoading, setInitialLoading] = useState(true);
  const [initialError, setInitialError] = useState<string | null>(null);
  const [liveEvents, setLiveEvents] = useState<DesignTimeEvent[]>([]);

  const poll = usePolling<WorkflowsResponse>({
    fetcher: fetchWorkflows,
    interval: 2000,
    enabled: !initialLoading && !initialError,
  });

  const workflow: AppWorkflow | undefined = useMemo(() => {
    return poll.data?.workflows.find((w) => w.tenant === tenant);
  }, [poll.data, tenant]);

  // Live SSE subscription for real-time verification progress
  useEffect(() => {
    const cleanup = subscribeDesignTimeEvents((event) => {
      if (event.tenant === tenant) {
        setLiveEvents((prev) => [...prev.slice(-99), event]);
      }
    });
    return cleanup;
  }, [tenant]);

  const loadInitial = useCallback(async () => {
    setInitialLoading(true);
    setInitialError(null);
    try {
      await fetchWorkflows();
    } catch (err) {
      setInitialError(err instanceof Error ? err.message : "Failed to load workflow");
    } finally {
      setInitialLoading(false);
    }
  }, []);

  useEffect(() => {
    loadInitial();
  }, [loadInitial]);

  if (initialLoading) return <DetailSkeleton />;
  if (initialError)
    return <ErrorDisplay title="Cannot load workflow" message={initialError} retry={loadInitial} />;
  if (!workflow)
    return (
      <ErrorDisplay
        title="Workflow not found"
        message={`No workflow found for app "${tenant}"`}
        retry={loadInitial}
      />
    );

  return (
    <div>
      {/* Breadcrumb */}
      <div className="mb-4">
        <Link href="/workflows" className="text-sm text-gray-500 hover:text-gray-300 transition-colors">
          Workflows
        </Link>
        <span className="text-gray-600 mx-2">/</span>
        <span className="text-sm text-gray-300">{tenant}</span>
      </div>

      {/* Header */}
      <div className="flex items-center justify-between mb-6">
        <div>
          <h1 className="text-2xl font-bold text-gray-100">{tenant}</h1>
          <p className="text-sm text-gray-500 mt-1">
            {workflow.entities.length} {workflow.entities.length === 1 ? "entity" : "entities"}
            {workflow.runtime_events_count > 0 && (
              <span className="ml-2">
                &middot; {workflow.runtime_events_count} runtime events
              </span>
            )}
          </p>
        </div>
      </div>

      {/* Status banner */}
      <StatusBanner status={workflow.status} />

      {/* Entity workflows */}
      <div className="space-y-6">
        {workflow.entities.map((entity) => {
          const entityDone = entity.steps.every(
            (s) => s.status === "completed" || s.status === "failed",
          );
          const entityFailed = entity.steps.some(
            (s) => s.status === "failed" || s.passed === false,
          );

          return (
            <div
              key={entity.entity_type}
              className={`bg-gray-900 border rounded-lg p-5 ${
                entityFailed
                  ? "border-red-800/50"
                  : entityDone
                  ? "border-gray-800"
                  : "border-gray-800"
              }`}
            >
              <div className="flex items-center justify-between mb-4">
                <div className="flex items-center gap-3">
                  <h3 className="text-base font-semibold text-gray-200 font-mono">
                    {entity.entity_type}
                  </h3>
                  {entityDone && !entityFailed && (
                    <span className="text-xs bg-green-900/50 text-green-400 px-2 py-0.5 rounded">
                      Verified
                    </span>
                  )}
                  {entityFailed && (
                    <span className="text-xs bg-red-900/50 text-red-400 px-2 py-0.5 rounded">
                      Failed
                    </span>
                  )}
                  {!entityDone && !entityFailed && (
                    <span className="text-xs bg-yellow-900/50 text-yellow-400 px-2 py-0.5 rounded">
                      In Progress
                    </span>
                  )}
                </div>
                <Link
                  href={`/specs/${entity.entity_type}`}
                  className="text-xs text-blue-400 hover:text-blue-300"
                >
                  View Spec
                </Link>
              </div>

              <WorkflowTimeline steps={entity.steps} entityType={entity.entity_type} />
            </div>
          );
        })}
      </div>

      {/* Live event stream */}
      {liveEvents.length > 0 && (
        <div className="mt-8">
          <h2 className="text-lg font-semibold text-gray-200 mb-4">Live Events</h2>
          <div className="bg-gray-900 border border-gray-800 rounded-lg overflow-hidden">
            <div className="max-h-64 overflow-y-auto">
              {liveEvents.slice().reverse().map((event, i) => (
                <div
                  key={i}
                  className="flex items-center gap-3 px-4 py-2.5 border-b border-gray-800/50 last:border-b-0"
                >
                  <div className={`w-1.5 h-1.5 rounded-full flex-shrink-0 ${
                    event.passed === true ? "bg-green-400" :
                    event.passed === false ? "bg-red-400" :
                    "bg-blue-400"
                  }`} />
                  <span className="text-xs font-mono text-gray-400 flex-shrink-0">
                    {event.entity_type}
                  </span>
                  <span className="text-xs text-gray-300 truncate">
                    {event.summary}
                  </span>
                  {event.timestamp && (
                    <span className="text-xs font-mono text-gray-600 flex-shrink-0 ml-auto">
                      {new Date(event.timestamp).toLocaleTimeString("en-US", {
                        hour: "2-digit",
                        minute: "2-digit",
                        second: "2-digit",
                        hour12: false,
                      })}
                    </span>
                  )}
                </div>
              ))}
            </div>
          </div>
        </div>
      )}
    </div>
  );
}
