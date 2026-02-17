"use client";

import { useEffect, useState, useCallback, useMemo } from "react";
import { fetchSpecs, fetchEntities, fetchVerificationStatus } from "@/lib/api";
import { usePolling, useRelativeTime } from "@/lib/hooks";
import type { SpecSummary, EntitySummary, AllVerificationStatus } from "@/lib/types";
import SpecCard from "@/components/SpecCard";
import StatusBadge from "@/components/StatusBadge";
import ErrorDisplay from "@/components/ErrorDisplay";
import Link from "next/link";

function DashboardSkeleton() {
  return (
    <div className="animate-pulse">
      <div className="h-7 bg-gray-800 rounded w-40 mb-2" />
      <div className="h-4 bg-gray-800/60 rounded w-72 mb-8" />

      <div className="grid grid-cols-3 gap-4 mb-8">
        {[0, 1, 2].map((i) => (
          <div key={i} className="bg-gray-900 border border-gray-800 rounded-lg p-4">
            <div className="h-4 bg-gray-800 rounded w-24 mb-2" />
            <div className="h-9 bg-gray-800 rounded w-12" />
          </div>
        ))}
      </div>

      <div className="h-5 bg-gray-800 rounded w-16 mb-4" />
      <div className="grid grid-cols-1 md:grid-cols-2 lg:grid-cols-3 gap-4 mb-8">
        {[0, 1, 2].map((i) => (
          <div key={i} className="bg-gray-900 border border-gray-800 rounded-lg p-5 h-48" />
        ))}
      </div>

      <div className="h-5 bg-gray-800 rounded w-20 mb-4" />
      <div className="bg-gray-900 border border-gray-800 rounded-lg p-4 space-y-3">
        {[0, 1, 2, 3].map((i) => (
          <div key={i} className="h-10 bg-gray-800/50 rounded" />
        ))}
      </div>
    </div>
  );
}

function EmptyState() {
  return (
    <div className="flex items-center justify-center min-h-[256px]">
      <div className="text-center max-w-md">
        <div className="inline-flex items-center justify-center w-12 h-12 rounded-full bg-gray-800 border border-gray-700 mb-4">
          <svg className="w-6 h-6 text-gray-500" fill="none" stroke="currentColor" viewBox="0 0 24 24">
            <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={1.5} d="M20 7l-8-4-8 4m16 0l-8 4m8-4v10l-8 4m0-10L4 7m8 4v10M4 7v10l8 4" />
          </svg>
        </div>
        <h3 className="text-lg font-semibold text-gray-200 mb-1">No specs loaded</h3>
        <p className="text-sm text-gray-400">
          Start the Temper server with{" "}
          <code className="font-mono text-xs bg-gray-800 px-1.5 py-0.5 rounded">
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

  // Collapse when all done
  if (allDone) return null;

  const progressPct = total > 0 ? Math.round((done / total) * 100) : 0;

  return (
    <div className="bg-gray-900 border border-gray-800 rounded-lg p-4 mb-6">
      <div className="flex items-center justify-between mb-3">
        <div className="flex items-center gap-2">
          <div className="w-2 h-2 bg-yellow-400 rounded-full animate-pulse" />
          <span className="text-sm font-medium text-gray-200">Verification in progress</span>
        </div>
        <span className="text-xs text-gray-500">
          {done} of {total} entities verified
        </span>
      </div>

      {/* Progress bar */}
      <div className="h-1.5 bg-gray-800 rounded-full mb-3 overflow-hidden">
        <div
          className="h-full bg-blue-500 rounded-full transition-all duration-500"
          style={{ width: `${progressPct}%` }}
        />
      </div>

      {/* Entity status dots */}
      <div className="flex flex-wrap gap-2">
        {verificationStatus.entities.map((entity) => {
          const dotColor: Record<string, string> = {
            pending: "bg-gray-500",
            running: "bg-yellow-400 animate-pulse",
            passed: "bg-green-400",
            failed: "bg-red-400",
            partial: "bg-amber-400",
          };
          return (
            <div key={`${entity.tenant}-${entity.entity_type}`} className="flex items-center gap-1.5">
              <div className={`w-2 h-2 rounded-full ${dotColor[entity.status] ?? "bg-gray-500"}`} />
              <span className="text-xs text-gray-400">{entity.entity_type}</span>
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

  // Filters
  const [typeFilter, setTypeFilter] = useState<string>("all");
  const [stateFilter, setStateFilter] = useState<string>("all");
  const [searchQuery, setSearchQuery] = useState("");
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

  // Derive unique tenants/types/states for filter dropdowns
  const tenants = useMemo(() => {
    const set = new Set<string>();
    for (const s of specs) {
      if (s.tenant) set.add(s.tenant);
    }
    return Array.from(set).sort();
  }, [specs]);

  const entityTypes = useMemo(() => {
    const set = new Set<string>();
    for (const e of entities) set.add(e.entity_type);
    return Array.from(set).sort();
  }, [entities]);

  const entityStates = useMemo(() => {
    const set = new Set<string>();
    for (const e of entities) {
      if (e.current_state) set.add(e.current_state);
    }
    return Array.from(set).sort();
  }, [entities]);

  // Filtered specs by tenant
  const filteredSpecs = useMemo(() => {
    if (tenantFilter === "all") return specs;
    return specs.filter((s) => s.tenant === tenantFilter);
  }, [specs, tenantFilter]);

  // Filtered entities
  const filteredEntities = useMemo(() => {
    let result = entities;
    if (typeFilter !== "all") {
      result = result.filter((e) => e.entity_type === typeFilter);
    }
    if (stateFilter !== "all") {
      result = result.filter((e) => (e.current_state ?? e.actor_status) === stateFilter);
    }
    if (searchQuery) {
      const q = searchQuery.toLowerCase();
      result = result.filter(
        (e) =>
          e.entity_id.toLowerCase().includes(q) ||
          e.entity_type.toLowerCase().includes(q),
      );
    }
    return result;
  }, [entities, typeFilter, stateFilter, searchQuery]);

  if (initialLoading) return <DashboardSkeleton />;
  if (initialError) return <ErrorDisplay title="Cannot load dashboard" message={initialError} retry={loadInitial} />;
  if (specs.length === 0 && entities.length === 0) return <EmptyState />;

  const entityCounts = entities.reduce<Record<string, number>>((acc, e) => {
    acc[e.entity_type] = (acc[e.entity_type] || 0) + 1;
    return acc;
  }, {});

  return (
    <div>
      {/* Header */}
      <div className="flex items-center justify-between mb-8">
        <div>
          <h1 className="text-2xl font-bold text-gray-100">Dashboard</h1>
          <p className="text-sm text-gray-500 mt-1">
            Overview of loaded specs and active entities
          </p>
        </div>
        <div className="flex items-center gap-4">
          {/* Tenant selector */}
          {tenants.length > 0 && (
            <select
              value={tenantFilter}
              onChange={(e) => setTenantFilter(e.target.value)}
              className="bg-gray-900 border border-gray-700 text-gray-300 text-xs rounded-md px-2 py-1.5 focus:border-blue-500 focus:outline-none"
            >
              <option value="all">All tenants</option>
              {tenants.map((t) => (
                <option key={t} value={t}>{t}</option>
              ))}
            </select>
          )}
          {/* Last updated indicator */}
          {lastUpdated && (
            <span className="text-xs text-gray-500">
              Updated {lastUpdated}
            </span>
          )}
        </div>
      </div>

      {/* Design-time progress panel */}
      {buildInProgress && verificationStatus && (
        <DesignTimeProgress verificationStatus={verificationStatus} />
      )}

      {/* Stats row */}
      <div className="grid grid-cols-3 gap-4 mb-8">
        <div className="bg-gray-900 border border-gray-800 rounded-lg p-4">
          <div className="text-sm text-gray-500">Loaded Specs</div>
          <div className="text-3xl font-bold font-mono text-gray-100 mt-1">
            {filteredSpecs.length}
          </div>
        </div>
        <div className="bg-gray-900 border border-gray-800 rounded-lg p-4">
          <div className="text-sm text-gray-500">Active Entities</div>
          <div className="text-3xl font-bold font-mono text-gray-100 mt-1">
            {entities.length}
          </div>
        </div>
        <div className="bg-gray-900 border border-gray-800 rounded-lg p-4">
          <div className="text-sm text-gray-500">Entity Types</div>
          <div className="text-3xl font-bold font-mono text-gray-100 mt-1">
            {Object.keys(entityCounts).length}
          </div>
        </div>
      </div>

      {/* Spec cards grouped by tenant */}
      <div className="mb-8">
        <h2 className="text-lg font-semibold text-gray-200 mb-4">Specs</h2>
        {(() => {
          const grouped = new Map<string, SpecSummary[]>();
          for (const spec of filteredSpecs) {
            const key = spec.tenant ?? "default";
            if (!grouped.has(key)) grouped.set(key, []);
            grouped.get(key)!.push(spec);
          }
          const entries = Array.from(grouped.entries()).sort(([a], [b]) => a.localeCompare(b));
          return entries.map(([tenant, tenantSpecs]) => (
            <div key={tenant} className="mb-6">
              <div className="flex items-center gap-2 mb-3">
                <Link
                  href={`/workflows/${tenant}`}
                  className="text-sm font-semibold text-gray-300 uppercase tracking-wide hover:text-blue-400 transition-colors"
                >
                  {tenant}
                </Link>
                <span className="text-xs text-gray-600">{tenantSpecs.length} {tenantSpecs.length === 1 ? "entity" : "entities"}</span>
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

      {/* Entity list */}
      <div>
        <div className="flex items-center justify-between mb-4">
          <h2 className="text-lg font-semibold text-gray-200">Entities</h2>
          {entityPoll.error && (
            <span className="text-xs text-red-400">Polling error</span>
          )}
        </div>

        {/* Filters */}
        <div className="flex items-center gap-3 mb-4">
          <input
            type="text"
            placeholder="Search by ID..."
            value={searchQuery}
            onChange={(e) => setSearchQuery(e.target.value)}
            className="bg-gray-900 border border-gray-700 text-gray-300 text-xs rounded-md px-3 py-1.5 w-48 focus:border-blue-500 focus:outline-none placeholder-gray-600"
          />
          <select
            value={typeFilter}
            onChange={(e) => setTypeFilter(e.target.value)}
            className="bg-gray-900 border border-gray-700 text-gray-300 text-xs rounded-md px-2 py-1.5 focus:border-blue-500 focus:outline-none"
          >
            <option value="all">All types</option>
            {entityTypes.map((t) => (
              <option key={t} value={t}>{t}</option>
            ))}
          </select>
          <select
            value={stateFilter}
            onChange={(e) => setStateFilter(e.target.value)}
            className="bg-gray-900 border border-gray-700 text-gray-300 text-xs rounded-md px-2 py-1.5 focus:border-blue-500 focus:outline-none"
          >
            <option value="all">All states</option>
            {entityStates.map((s) => (
              <option key={s} value={s}>{s}</option>
            ))}
          </select>
          {(typeFilter !== "all" || stateFilter !== "all" || searchQuery) && (
            <button
              onClick={() => {
                setTypeFilter("all");
                setStateFilter("all");
                setSearchQuery("");
              }}
              className="text-xs text-gray-500 hover:text-gray-300"
            >
              Clear filters
            </button>
          )}
        </div>

        {entities.length === 0 ? (
          <div className="bg-gray-900 border border-gray-800 rounded-lg p-8 text-center">
            <p className="text-sm text-gray-400">
              No active entities. Create one with{" "}
              <code className="font-mono text-xs bg-gray-800 px-1.5 py-0.5 rounded">
                POST /tdata/&#123;EntitySet&#125;
              </code>
            </p>
          </div>
        ) : filteredEntities.length === 0 ? (
          <div className="bg-gray-900 border border-gray-800 rounded-lg p-8 text-center">
            <p className="text-sm text-gray-400">No entities match the current filters.</p>
          </div>
        ) : (
          <div className="bg-gray-900 border border-gray-800 rounded-lg overflow-hidden">
            <table className="w-full text-sm">
              <thead>
                <tr className="border-b border-gray-800">
                  <th className="text-left px-4 py-3 text-gray-500 font-medium">Type</th>
                  <th className="text-left px-4 py-3 text-gray-500 font-medium">ID</th>
                  <th className="text-left px-4 py-3 text-gray-500 font-medium">Status</th>
                  <th className="text-right px-4 py-3 text-gray-500 font-medium"></th>
                </tr>
              </thead>
              <tbody>
                {filteredEntities.map((entity) => (
                  <tr
                    key={`${entity.entity_type}-${entity.entity_id}`}
                    className="border-b border-gray-800/50 hover:bg-gray-800/30 transition-colors"
                  >
                    <td className="px-4 py-3 font-mono text-gray-300">
                      {entity.entity_type}
                    </td>
                    <td className="px-4 py-3 font-mono text-gray-400">
                      {entity.entity_id}
                    </td>
                    <td className="px-4 py-3">
                      <StatusBadge status={entity.current_state ?? entity.actor_status} />
                    </td>
                    <td className="px-4 py-3 text-right">
                      <Link
                        href={`/entities/${entity.entity_type}/${entity.entity_id}`}
                        className="text-blue-400 hover:text-blue-300 text-xs"
                      >
                        Inspect
                      </Link>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </div>
    </div>
  );
}
