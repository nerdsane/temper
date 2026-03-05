"use client";

import { useEffect, useState, useCallback, useMemo, useRef } from "react";
import { fetchSpecs, fetchEntities, fetchVerificationStatus, subscribeDesignTimeEvents, subscribeEntityEvents } from "@/lib/api";
import { usePolling, useRelativeTime } from "@/lib/hooks";
import type { SpecSummary, EntitySummary, AllVerificationStatus } from "@/lib/types";
import SpecCard from "@/components/SpecCard";
import ErrorDisplay from "@/components/ErrorDisplay";
import Link from "next/link";

function DashboardSkeleton() {
  return (
    <div className="animate-pulse">
      <div className="h-6 bg-zinc-800/60 rounded w-36 mb-1.5" />
      <div className="h-3.5 bg-zinc-800/40 rounded w-64 mb-6" />

      <div className="grid grid-cols-3 gap-3 mb-6">
        {[0, 1, 2].map((i) => (
          <div key={i} className="glass rounded-lg p-4">
            <div className="h-3 bg-zinc-800/50 rounded w-20 mb-2" />
            <div className="h-8 bg-zinc-800/50 rounded w-10" />
          </div>
        ))}
      </div>

      <div className="h-4 bg-zinc-800/50 rounded w-14 mb-3" />
      <div className="grid grid-cols-1 md:grid-cols-2 lg:grid-cols-3 gap-3 mb-6">
        {[0, 1, 2].map((i) => (
          <div key={i} className="bg-[#111115] rounded-lg p-4 h-44" />
        ))}
      </div>
    </div>
  );
}

function EmptyState() {
  return (
    <div className="flex items-center justify-center min-h-[256px]">
      <div className="text-center max-w-md">
        <div className="inline-flex items-center justify-center w-10 h-10 rounded-full bg-white/[0.04] mb-4">
          <svg className="w-5 h-5 text-zinc-600" fill="none" stroke="currentColor" viewBox="0 0 24 24">
            <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={1.5} d="M20 7l-8-4-8 4m16 0l-8 4m8-4v10l-8 4m0-10L4 7m8 4v10M4 7v10l8 4" />
          </svg>
        </div>
        <h3 className="text-base font-semibold text-zinc-200 mb-1">No specs loaded</h3>
        <p className="text-sm text-zinc-500">
          Start the Temper server with{" "}
          <code className="font-mono text-[11px] bg-white/[0.04] px-1.5 py-0.5 rounded">
            temper serve --specs-dir &lt;path&gt;
          </code>
        </p>
      </div>
    </div>
  );
}

function DesignTimeProgress({ verificationStatus }: { verificationStatus: AllVerificationStatus }) {
  const total = verificationStatus.pending + verificationStatus.running +
    verificationStatus.passed + verificationStatus.failed + verificationStatus.partial;

  if (total === 0) return null;

  const done = verificationStatus.passed + verificationStatus.failed + verificationStatus.partial;
  const allDone = verificationStatus.pending === 0 && verificationStatus.running === 0;

  const progressPct = total > 0 ? Math.round((done / total) * 100) : 0;

  // Track previous entity dot statuses for flash animation
  const prevDotStatusRef = useRef<Record<string, string>>({});

  // Track "all done" state with delay before hiding
  const [showComplete, setShowComplete] = useState(false);
  const allDoneTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  useEffect(() => {
    if (allDone && total > 0) {
      setShowComplete(true);
      allDoneTimerRef.current = setTimeout(() => setShowComplete(false), 3000);
    } else {
      setShowComplete(false);
      if (allDoneTimerRef.current) clearTimeout(allDoneTimerRef.current);
    }
    return () => { if (allDoneTimerRef.current) clearTimeout(allDoneTimerRef.current); };
  }, [allDone, total]);

  // Hide after the 3s "all done" display
  if (allDone && !showComplete) return null;

  return (
    <div className="bg-[#111115] rounded-lg p-3.5 mb-5">
      <div className="flex items-center justify-between mb-2.5">
        <div className="flex items-center gap-2">
          {allDone ? (
            <svg className="w-3.5 h-3.5 text-teal-400" fill="none" stroke="currentColor" viewBox="0 0 24 24">
              <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M5 13l4 4L19 7" />
            </svg>
          ) : (
            <div className="w-1.5 h-1.5 bg-lime-400 rounded-full animate-pulse" />
          )}
          <span className="text-sm font-medium text-zinc-300">
            {allDone ? "All entities verified" : "Verification in progress"}
          </span>
        </div>
        <span className="text-xs text-zinc-600">
          {done} of {total} entities verified
        </span>
      </div>

      {/* Progress bar */}
      <div className="h-1 bg-white/[0.04] rounded-full mb-2.5 overflow-hidden">
        <div
          className="h-full bg-teal-500 rounded-full transition-all duration-500"
          style={{ width: `${progressPct}%` }}
        />
      </div>

      {/* Entity status dots */}
      <div className="flex flex-wrap gap-2">
        {verificationStatus.entities.map((entity) => {
          const key = `${entity.tenant}-${entity.entity_type}`;
          const prevStatus = prevDotStatusRef.current[key];
          const changed = prevStatus !== undefined && prevStatus !== entity.status;
          prevDotStatusRef.current[key] = entity.status;

          const dotColor: Record<string, string> = {
            pending: "bg-zinc-600",
            running: "bg-amber-400 animate-pulse",
            passed: "bg-teal-400",
            failed: "bg-pink-400",
            partial: "bg-amber-400",
          };
          const flashClass = changed
            ? (entity.status === "failed" ? "animate-flash-pink" : "animate-flash-teal")
            : "";

          return (
            <div key={key} className={`flex items-center gap-1.5 rounded px-1 ${flashClass}`}>
              <div className={`w-1.5 h-1.5 rounded-full transition-colors duration-300 ${dotColor[entity.status] ?? "bg-zinc-600"}`} />
              <span className="text-xs text-zinc-500 font-mono">{entity.entity_type}</span>
            </div>
          );
        })}
      </div>
    </div>
  );
}

export default function Dashboard() {
  const [initialLoading, setInitialLoading] = useState(true);
  const [initialError, setInitialError] = useState<string | null>(null);

  const [tenantFilter, setTenantFilter] = useState<string>("all");

  // Poll specs every 3s during build to pick up verification_status updates
  const specPoll = usePolling<SpecSummary[]>({
    fetcher: fetchSpecs,
    interval: 3000,
    enabled: !initialLoading && !initialError,
  });
  const specs = useMemo(() => specPoll.data ?? [], [specPoll.data]);

  // Auto-refresh entities every 5s
  const entityPoll = usePolling<EntitySummary[]>({
    fetcher: fetchEntities,
    interval: 5000,
    enabled: !initialLoading && !initialError,
  });
  const entities = useMemo(() => entityPoll.data ?? [], [entityPoll.data]);
  const lastUpdated = useRelativeTime(entityPoll.lastUpdated);

  // Poll verification status every 2s during build
  const verifyPoll = usePolling<AllVerificationStatus>({
    fetcher: fetchVerificationStatus,
    interval: 2000,
    enabled: !initialLoading && !initialError,
  });
  const verificationStatus = verifyPoll.data;

  // Check if any entity is still pending/running (to show progress panel)
  const buildInProgress = useMemo(() => {
    if (!verificationStatus) return false;
    return verificationStatus.pending > 0 || verificationStatus.running > 0;
  }, [verificationStatus]);

  // SSE subscriptions for real-time reactivity
  useEffect(() => {
    if (initialLoading || initialError) return;
    const cleanupDesign = subscribeDesignTimeEvents(() => {
      specPoll.refresh();
      verifyPoll.refresh();
    });
    const cleanupEntity = subscribeEntityEvents(() => {
      entityPoll.refresh();
      setEntityHighlight("animate-highlight-new");
    });
    return () => { cleanupDesign(); cleanupEntity(); };
  }, [initialLoading, initialError]); // eslint-disable-line react-hooks/exhaustive-deps

  // Stat counter highlight on entity count changes
  const prevEntityCountRef = useRef<number>(0);
  const [entityHighlight, setEntityHighlight] = useState("");

  useEffect(() => {
    if (entities.length !== prevEntityCountRef.current && prevEntityCountRef.current > 0) {
      setEntityHighlight("animate-highlight-new");
    }
    prevEntityCountRef.current = entities.length;
  }, [entities.length]);

  const loadInitial = useCallback(async () => {
    setInitialLoading(true);
    setInitialError(null);
    try {
      await fetchSpecs();
    } catch (err) {
      setInitialError(err instanceof Error ? err.message : "Failed to load dashboard data");
    } finally {
      setInitialLoading(false);
    }
  }, []);

  useEffect(() => {
    loadInitial();
  }, [loadInitial]);

  // Derive unique tenants for filter dropdown (hide internal platform tenant)
  const tenants = useMemo(() => {
    const set = new Set<string>();
    for (const s of specs) {
      if (s.tenant && s.tenant !== "temper-system") set.add(s.tenant);
    }
    return Array.from(set).sort();
  }, [specs]);

  // Filtered specs by tenant (always exclude temper-system)
  const filteredSpecs = useMemo(() => {
    const visible = specs.filter((s) => s.tenant !== "temper-system");
    if (tenantFilter === "all") return visible;
    return visible.filter((s) => s.tenant === tenantFilter);
  }, [specs, tenantFilter]);

  if (initialLoading) return <DashboardSkeleton />;
  if (initialError) return <ErrorDisplay title="Cannot load dashboard" message={initialError} retry={loadInitial} />;
  if (specs.length === 0 && entities.length === 0) return <EmptyState />;

  const entityCounts = entities.reduce<Record<string, number>>((acc, e) => {
    acc[e.entity_type] = (acc[e.entity_type] || 0) + 1;
    return acc;
  }, {});

  return (
    <div className="animate-fade-in">
      {/* Header */}
      <div className="flex items-center justify-between mb-6">
        <div>
          <h1 className="text-2xl font-bold text-zinc-100 tracking-tight font-display">Dashboard</h1>
          <p className="text-sm text-zinc-600 mt-0.5">
            Overview of loaded specs and active entities
          </p>
        </div>
        <div className="flex items-center gap-3">
          {/* Tenant selector */}
          {tenants.length > 0 && (
            <select
              value={tenantFilter}
              onChange={(e) => setTenantFilter(e.target.value)}
              className="bg-[#111115] text-zinc-400 text-xs rounded-md px-2 py-1.5 focus:outline-none"
            >
              <option value="all">All tenants</option>
              {tenants.map((t) => (
                <option key={t} value={t}>{t}</option>
              ))}
            </select>
          )}
          {/* Last updated indicator */}
          {lastUpdated && (
            <span className="text-xs text-zinc-600">
              Updated {lastUpdated}
            </span>
          )}
        </div>
      </div>

      {/* Design-time progress panel */}
      {verificationStatus && (
        <DesignTimeProgress verificationStatus={verificationStatus} />
      )}

      {/* Stats row */}
      <div className="grid grid-cols-3 gap-4 mb-6">
        <div className="glass rounded-lg p-4">
          <div className="text-xs text-zinc-600">Loaded Specs</div>
          <div className="text-4xl font-bold font-mono text-zinc-100 mt-0.5">
            {filteredSpecs.length}
          </div>
        </div>
        <div
          className={`glass rounded-lg p-4 ${entityHighlight}`}
          onAnimationEnd={() => setEntityHighlight("")}
        >
          <div className="text-xs text-zinc-600">Active Entities</div>
          <div className="text-4xl font-bold font-mono text-zinc-100 mt-0.5">
            {entities.length}
          </div>
        </div>
        <div className="glass rounded-lg p-4">
          <div className="text-xs text-zinc-600">Entity Types</div>
          <div className="text-4xl font-bold font-mono text-zinc-100 mt-0.5">
            {Object.keys(entityCounts).length}
          </div>
        </div>
      </div>

      {/* Spec cards grouped by tenant */}
      <div className="mb-6">
        <h2 className="text-base font-semibold text-zinc-200 mb-3 tracking-tight">Specs</h2>
        {(() => {
          const grouped = new Map<string, SpecSummary[]>();
          for (const spec of filteredSpecs) {
            const key = spec.tenant ?? "default";
            if (!grouped.has(key)) grouped.set(key, []);
            grouped.get(key)!.push(spec);
          }
          const entries = Array.from(grouped.entries()).sort(([a], [b]) => a.localeCompare(b));
          return entries.map(([tenant, tenantSpecs]) => (
            <div key={tenant} className="mb-5">
              <div className="flex items-center gap-2 mb-2.5">
                <Link
                  href={`/workflows/${tenant}`}
                  className="text-xs font-medium text-zinc-400 uppercase tracking-widest hover:text-teal-400 transition-colors"
                >
                  {tenant}
                </Link>
                <span className="text-xs text-zinc-700">{tenantSpecs.length} {tenantSpecs.length === 1 ? "entity" : "entities"}</span>
              </div>
              <div className="grid grid-cols-1 md:grid-cols-2 lg:grid-cols-3 gap-4">
                {tenantSpecs.map((spec) => (
                  <SpecCard key={`${spec.tenant}-${spec.entity_type}`} spec={spec} />
                ))}
              </div>
            </div>
          ));
        })()}
      </div>
    </div>
  );
}
