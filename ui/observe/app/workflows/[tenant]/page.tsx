"use client";

import { useEffect, useState, useMemo, useCallback } from "react";
import { useParams } from "next/navigation";
import { fetchWorkflows, fetchVerificationStatus, subscribeDesignTimeEvents } from "@/lib/api";
import { usePolling } from "@/lib/hooks";
import type { WorkflowsResponse, AppWorkflow, DesignTimeEvent } from "@/lib/types";
import WorkflowTimeline from "@/components/WorkflowTimeline";
import type { StepDetailsMap } from "@/components/WorkflowTimeline";
import ErrorDisplay from "@/components/ErrorDisplay";
import Link from "next/link";

/** Map verification level names from the API to workflow step names. */
function levelToStepName(level: string): string | null {
  if (level.includes("Symbolic")) return "L0_symbolic";
  if (level.includes("Model Check")) return "L1_model_check";
  if (level.includes("Simulation")) return "L2_simulation";
  if (level.includes("Property")) return "L3_property_test";
  return null;
}

function DetailSkeleton() {
  return (
    <div className="animate-pulse">
      <div className="h-3.5 bg-zinc-800/40 rounded w-20 mb-3" />
      <div className="h-6 bg-zinc-800/60 rounded w-44 mb-1.5" />
      <div className="h-3.5 bg-zinc-800/40 rounded w-64 mb-6" />
      <div className="space-y-4">
        {[0, 1, 2].map((i) => (
          <div key={i} className="bg-[#111115] rounded-lg p-4 h-56" />
        ))}
      </div>
    </div>
  );
}

function StatusBanner({ status }: { status: string }) {
  const configs: Record<string, { bg: string; text: string; label: string }> = {
    loading: { bg: "bg-teal-500/5", text: "text-teal-400", label: "Loading specs..." },
    verifying: { bg: "bg-yellow-500/5", text: "text-yellow-400", label: "Verification in progress" },
    completed: { bg: "bg-teal-500/5", text: "text-teal-400", label: "All entities verified and deployed" },
    failed: { bg: "bg-pink-500/5", text: "text-pink-400", label: "Some entities failed verification" },
  };
  const c = configs[status] ?? configs.loading;
  const isActive = status === "loading" || status === "verifying";

  return (
    <div className={`${c.bg} rounded-lg px-3.5 py-2.5 mb-5 flex items-center gap-2.5`}>
      {isActive && <div className="w-1.5 h-1.5 bg-amber-400 rounded-full animate-pulse" />}
      {!isActive && (
        <span className={`text-sm ${c.text}`}>
          {status === "completed" ? "\u2713" : "\u2717"}
        </span>
      )}
      <span className={`text-[13px] font-medium ${c.text}`}>{c.label}</span>
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

  // Build a map of entity_type → step_name → VerificationDetail[] from verification status
  const [entityStepDetails, setEntityStepDetails] = useState<Record<string, StepDetailsMap>>({});

  useEffect(() => {
    if (!workflow) return;
    // Only fetch if any entity has failures
    const hasFailed = workflow.entities.some((e) =>
      e.steps.some((s) => s.passed === false),
    );
    if (!hasFailed) return;

    fetchVerificationStatus()
      .then((status) => {
        const map: Record<string, StepDetailsMap> = {};
        for (const entity of status.entities) {
          if (entity.tenant !== tenant) continue;
          if (!entity.levels) continue;
          const stepMap: StepDetailsMap = {};
          for (const level of entity.levels) {
            if (!level.passed && Array.isArray(level.details) && level.details.length > 0) {
              // Map level name to step name (e.g. "Level 2: Deterministic Simulation" → "L2_simulation")
              const stepName = levelToStepName(level.level);
              if (stepName) {
                stepMap[stepName] = level.details;
              }
            }
          }
          if (Object.keys(stepMap).length > 0) {
            map[entity.entity_type] = stepMap;
          }
        }
        setEntityStepDetails(map);
      })
      .catch(() => {
        // Ignore — details are optional enrichment
      });
  }, [workflow, tenant]);

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
    <div className="animate-fade-in">
      {/* Breadcrumb */}
      <div className="mb-3">
        <Link href="/workflows" className="text-[12px] text-zinc-600 hover:text-zinc-400 transition-colors">
          Workflows
        </Link>
        <span className="text-zinc-700 mx-1.5">/</span>
        <span className="text-[12px] text-zinc-400">{tenant}</span>
      </div>

      {/* Header */}
      <div className="flex items-center justify-between mb-5">
        <div>
          <h1 className="text-2xl font-bold text-zinc-100 tracking-tight font-display">{tenant}</h1>
          <p className="text-[13px] text-zinc-600 mt-0.5">
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
      <div className="space-y-4">
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
              className={`bg-[#111115] rounded-lg p-4 ${
                entityFailed ? "bg-pink-500/5" : ""
              }`}
            >
              <div className="flex items-center justify-between mb-3">
                <div className="flex items-center gap-2.5">
                  <h3 className="text-sm font-semibold text-zinc-200 font-mono tracking-tight">
                    {entity.entity_type}
                  </h3>
                  {entityDone && !entityFailed && (
                    <span className="text-[10px] bg-teal-500/10 text-teal-400 px-2 py-0.5 rounded-full font-mono">
                      Verified
                    </span>
                  )}
                  {entityFailed && (
                    <span className="text-[10px] bg-pink-500/10 text-pink-400 px-2 py-0.5 rounded-full font-mono">
                      Failed
                    </span>
                  )}
                  {!entityDone && !entityFailed && (
                    <span className="text-[10px] bg-yellow-500/10 text-yellow-400 px-2 py-0.5 rounded-full font-mono">
                      In Progress
                    </span>
                  )}
                </div>
                <Link
                  href={`/specs/${entity.entity_type}`}
                  className="text-[11px] text-teal-400 hover:text-teal-300 transition-colors"
                >
                  View Spec
                </Link>
              </div>

              <WorkflowTimeline
                steps={entity.steps}
                entityType={entity.entity_type}
                stepDetails={entityStepDetails[entity.entity_type]}
              />
            </div>
          );
        })}
      </div>

      {/* Live event stream */}
      {liveEvents.length > 0 && (
        <div className="mt-6">
          <h2 className="text-[15px] font-semibold text-zinc-200 mb-3 tracking-tight">Live Events</h2>
          <div className="bg-[#111115] rounded-lg overflow-hidden">
            <div className="max-h-56 overflow-y-auto">
              {liveEvents.slice().reverse().map((event, i) => (
                <div
                  key={i}
                  className="flex items-center gap-2.5 px-3.5 py-2 border-b border-white/[0.03] last:border-b-0 animate-slide-in"
                >
                  <div className={`w-1 h-1 rounded-full flex-shrink-0 ${
                    event.passed === true ? "bg-teal-400" :
                    event.passed === false ? "bg-pink-400" :
                    "bg-teal-400"
                  }`} />
                  <span className="text-[11px] font-mono text-zinc-500 flex-shrink-0">
                    {event.entity_type}
                  </span>
                  <span className="text-[11px] text-zinc-400 truncate">
                    {event.summary}
                  </span>
                  {event.timestamp && (
                    <span className="text-[10px] font-mono text-zinc-700 flex-shrink-0 ml-auto">
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
