"use client";

import { useEffect, useState, useMemo } from "react";
import { fetchWorkflows } from "@/lib/api";
import { usePolling } from "@/lib/hooks";
import type { WorkflowsResponse, AppWorkflow } from "@/lib/types";
import ErrorDisplay from "@/components/ErrorDisplay";
import Link from "next/link";

function StatusIndicator({ status }: { status: string }) {
  const config: Record<string, { color: string; label: string; animate?: boolean }> = {
    loading: { color: "bg-teal-400", label: "Loading", animate: true },
    verifying: { color: "bg-yellow-400", label: "Verifying", animate: true },
    completed: { color: "bg-teal-400", label: "Verified" },
    failed: { color: "bg-pink-400", label: "Failed" },
  };
  const c = config[status] ?? config.loading;
  return (
    <div className="flex items-center gap-1.5">
      <div className={`w-1.5 h-1.5 rounded-full ${c.color} ${c.animate ? "animate-pulse" : ""}`} />
      <span className="text-[11px] text-zinc-500">{c.label}</span>
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
      className="block bg-[#111115] rounded-lg p-5 hover:bg-white/[0.02] transition-colors group"
    >
      <div className="flex items-center justify-between mb-2.5">
        <h3 className="text-base font-semibold text-zinc-100 tracking-tight">{workflow.tenant}</h3>
        <StatusIndicator status={workflow.status} />
      </div>

      {/* Entity count */}
      <div className="text-[13px] text-zinc-500 mb-2.5">
        {workflow.entities.length} {workflow.entities.length === 1 ? "entity" : "entities"}
        {workflow.runtime_events_count > 0 && (
          <span className="ml-2.5 text-zinc-600">
            {workflow.runtime_events_count} runtime events
          </span>
        )}
      </div>

      {/* Progress bar */}
      <div className="h-1 bg-white/[0.04] rounded-full overflow-hidden mb-2.5">
        <div
          className={`h-full rounded-full transition-all duration-500 ${
            workflow.status === "failed" || !allPassed
              ? "bg-pink-500"
              : workflow.status === "completed"
              ? "bg-teal-500"
              : "bg-teal-500"
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
            ? "bg-pink-400"
            : entityDone
            ? "bg-teal-400"
            : entityRunning
            ? "bg-amber-400 animate-pulse"
            : "bg-zinc-600";

          return (
            <div key={entity.entity_type} className="flex items-center gap-1.5">
              <div className={`w-1.5 h-1.5 rounded-full ${dotColor}`} />
              <span className="text-[11px] text-zinc-500 font-mono">{entity.entity_type}</span>
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
      <div className="h-6 bg-zinc-800/60 rounded w-36 mb-1.5" />
      <div className="h-3.5 bg-zinc-800/40 rounded w-72 mb-6" />
      <div className="grid grid-cols-1 md:grid-cols-2 gap-3">
        {[0, 1].map((i) => (
          <div key={i} className="bg-[#111115] rounded-lg p-4 h-36" />
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
    <div className="animate-fade-in">
      <div className="mb-6">
        <h1 className="text-2xl font-bold text-zinc-100 tracking-tight font-display">Workflows</h1>
        <p className="text-sm text-zinc-600 mt-0.5">
          App deployment workflows — spec loading, verification cascade, and runtime
        </p>
      </div>

      {workflows.length === 0 ? (
        <div className="flex items-center justify-center min-h-[256px]">
          <div className="text-center max-w-md">
            <div className="inline-flex items-center justify-center w-10 h-10 rounded-full bg-white/[0.04] mb-4">
              <svg className="w-5 h-5 text-zinc-600" fill="none" stroke="currentColor" viewBox="0 0 24 24">
                <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={1.5} d="M13 10V3L4 14h7v7l9-11h-7z" />
              </svg>
            </div>
            <h3 className="text-[15px] font-semibold text-zinc-200 mb-1">No app workflows</h3>
            <p className="text-[13px] text-zinc-500">
              Start the server with{" "}
              <code className="font-mono text-[11px] bg-white/[0.04] px-1.5 py-0.5 rounded">
                temper serve --app name=specs-dir
              </code>
            </p>
          </div>
        </div>
      ) : (
        <div className="grid grid-cols-1 md:grid-cols-2 gap-3">
          {workflows.map((w) => (
            <AppCard key={w.tenant} workflow={w} />
          ))}
        </div>
      )}
    </div>
  );
}
