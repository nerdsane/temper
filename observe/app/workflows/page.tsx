"use client";

import { useEffect, useState, useMemo } from "react";
import { fetchWorkflows } from "@/lib/api";
import { usePolling } from "@/lib/hooks";
import type { WorkflowsResponse, AppWorkflow } from "@/lib/types";
import ErrorDisplay from "@/components/ErrorDisplay";
import Link from "next/link";

function StatusIndicator({ status }: { status: string }) {
  const config: Record<string, { color: string; label: string; animate?: boolean }> = {
    loading: { color: "bg-blue-400", label: "Loading", animate: true },
    verifying: { color: "bg-yellow-400", label: "Verifying", animate: true },
    completed: { color: "bg-green-400", label: "Verified" },
    failed: { color: "bg-red-400", label: "Failed" },
  };
  const c = config[status] ?? config.loading;
  return (
    <div className="flex items-center gap-2">
      <div className={`w-2 h-2 rounded-full ${c.color} ${c.animate ? "animate-pulse" : ""}`} />
      <span className="text-xs text-gray-400">{c.label}</span>
    </div>
  );
}

function AppCard({ workflow }: { workflow: AppWorkflow }) {
  const totalSteps = workflow.entities.length * 7;
  const completedSteps = workflow.entities.reduce(
    (acc, e) => acc + e.steps.filter((s) => s.status === "completed" || s.status === "failed").length,
    0,
  );
  const progressPct = totalSteps > 0 ? Math.round((completedSteps / totalSteps) * 100) : 0;
  const allPassed = workflow.entities.every((e) =>
    e.steps.every((s) => s.status !== "failed" && s.passed !== false),
  );

  return (
    <Link
      href={`/workflows/${workflow.tenant}`}
      className="block bg-gray-900 border border-gray-800 rounded-lg p-5 hover:border-gray-600 transition-colors"
    >
      <div className="flex items-center justify-between mb-3">
        <h3 className="text-lg font-semibold text-gray-100">{workflow.tenant}</h3>
        <StatusIndicator status={workflow.status} />
      </div>

      {/* Entity count */}
      <div className="text-sm text-gray-400 mb-3">
        {workflow.entities.length} {workflow.entities.length === 1 ? "entity" : "entities"}
        {workflow.runtime_events_count > 0 && (
          <span className="ml-3 text-gray-500">
            {workflow.runtime_events_count} runtime events
          </span>
        )}
      </div>

      {/* Progress bar */}
      <div className="h-1.5 bg-gray-800 rounded-full overflow-hidden mb-3">
        <div
          className={`h-full rounded-full transition-all duration-500 ${
            workflow.status === "failed" || !allPassed
              ? "bg-red-500"
              : workflow.status === "completed"
              ? "bg-green-500"
              : "bg-blue-500"
          }`}
          style={{ width: `${progressPct}%` }}
        />
      </div>

      {/* Entity dots */}
      <div className="flex flex-wrap gap-2">
        {workflow.entities.map((entity) => {
          const entityDone = entity.steps.every(
            (s) => s.status === "completed" || s.status === "failed",
          );
          const entityFailed = entity.steps.some(
            (s) => s.status === "failed" || s.passed === false,
          );
          const entityRunning = entity.steps.some((s) => s.status === "running");
          const dotColor = entityFailed
            ? "bg-red-400"
            : entityDone
            ? "bg-green-400"
            : entityRunning
            ? "bg-yellow-400 animate-pulse"
            : "bg-gray-500";

          return (
            <div key={entity.entity_type} className="flex items-center gap-1.5">
              <div className={`w-2 h-2 rounded-full ${dotColor}`} />
              <span className="text-xs text-gray-400 font-mono">{entity.entity_type}</span>
            </div>
          );
        })}
      </div>
    </Link>
  );
}

function WorkflowsSkeleton() {
  return (
    <div className="animate-pulse">
      <div className="h-7 bg-gray-800 rounded w-40 mb-2" />
      <div className="h-4 bg-gray-800/60 rounded w-80 mb-8" />
      <div className="grid grid-cols-1 md:grid-cols-2 gap-4">
        {[0, 1].map((i) => (
          <div key={i} className="bg-gray-900 border border-gray-800 rounded-lg p-5 h-40" />
        ))}
      </div>
    </div>
  );
}

export default function WorkflowsPage() {
  const [initialLoading, setInitialLoading] = useState(true);
  const [initialError, setInitialError] = useState<string | null>(null);

  const poll = usePolling<WorkflowsResponse>({
    fetcher: fetchWorkflows,
    interval: 2000,
    enabled: !initialLoading && !initialError,
  });
  const workflows = useMemo(() => poll.data?.workflows ?? [], [poll.data]);

  useEffect(() => {
    fetchWorkflows()
      .then(() => setInitialLoading(false))
      .catch((err) => {
        setInitialError(err instanceof Error ? err.message : "Failed to load workflows");
        setInitialLoading(false);
      });
  }, []);

  if (initialLoading) return <WorkflowsSkeleton />;
  if (initialError)
    return (
      <ErrorDisplay
        title="Cannot load workflows"
        message={initialError}
        retry={() => window.location.reload()}
      />
    );

  return (
    <div>
      <div className="mb-8">
        <h1 className="text-2xl font-bold text-gray-100">Workflows</h1>
        <p className="text-sm text-gray-500 mt-1">
          App deployment workflows — spec loading, verification cascade, and runtime
        </p>
      </div>

      {workflows.length === 0 ? (
        <div className="flex items-center justify-center min-h-[256px]">
          <div className="text-center max-w-md">
            <div className="inline-flex items-center justify-center w-12 h-12 rounded-full bg-gray-800 border border-gray-700 mb-4">
              <svg className="w-6 h-6 text-gray-500" fill="none" stroke="currentColor" viewBox="0 0 24 24">
                <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={1.5} d="M13 10V3L4 14h7v7l9-11h-7z" />
              </svg>
            </div>
            <h3 className="text-lg font-semibold text-gray-200 mb-1">No app workflows</h3>
            <p className="text-sm text-gray-400">
              Start the server with{" "}
              <code className="font-mono text-xs bg-gray-800 px-1.5 py-0.5 rounded">
                temper serve --app name=specs-dir
              </code>
            </p>
          </div>
        </div>
      ) : (
        <div className="grid grid-cols-1 md:grid-cols-2 gap-4">
          {workflows.map((w) => (
            <AppCard key={w.tenant} workflow={w} />
          ))}
        </div>
      )}
    </div>
  );
}
