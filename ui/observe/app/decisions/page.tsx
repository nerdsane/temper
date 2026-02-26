"use client";

import { useEffect, useState, useCallback, useMemo } from "react";
import {
  fetchDecisions,
  fetchAllDecisions,
  approveDecision,
  denyDecision,
  subscribePendingDecisions,
  subscribeAllPendingDecisions,
  fetchSpecs,
} from "@/lib/api";
import { usePolling, useRelativeTime } from "@/lib/hooks";
import type {
  DecisionsResponse,
  PendingDecision,
  PolicyScope,
  SpecSummary,
} from "@/lib/types";
import ErrorDisplay from "@/components/ErrorDisplay";

const ALL_TENANTS = "__all__";

function StatCard({
  label,
  value,
  color,
}: {
  label: string;
  value: string | number;
  color?: string;
}) {
  return (
    <div className="glass rounded p-3.5">
      <div className="text-[12px] text-zinc-600">{label}</div>
      <div
        className={`text-4xl font-bold font-mono mt-0.5 ${color ?? "text-zinc-100"}`}
      >
        {value}
      </div>
    </div>
  );
}

const SCOPE_LABELS: Record<PolicyScope, string> = {
  narrow: "Narrow -- exact resource only",
  medium: "Medium -- same resource type",
  broad: "Broad -- all resources for action",
};

function DecisionCard({
  decision,
  onApprove,
  onDeny,
  acting,
  showTenant,
}: {
  decision: PendingDecision;
  onApprove: (id: string, scope: PolicyScope, tenant: string) => void;
  onDeny: (id: string, tenant: string) => void;
  acting: boolean;
  showTenant?: boolean;
}) {
  const [scope, setScope] = useState<PolicyScope>("narrow");
  const [showPolicy, setShowPolicy] = useState(false);
  const ts = new Date(decision.created_at);
  const timeStr = ts.toLocaleString();

  return (
    <div className="glass rounded p-4 animate-fade-in">
      <div className="flex items-start justify-between mb-3">
        <div className="flex items-center gap-2">
          <div className="w-2 h-2 rounded-full bg-pink-400 animate-pulse" />
          <span className="text-sm font-mono text-zinc-200">
            {decision.agent_id}
          </span>
          {showTenant && (
            <span className="text-[10px] font-mono px-1.5 py-0.5 rounded bg-zinc-800 text-zinc-500">
              {decision.tenant}
            </span>
          )}
        </div>
        <span className="text-[11px] text-zinc-600 font-mono">{timeStr}</span>
      </div>

      <div className="space-y-1.5 mb-3">
        <div className="flex items-center gap-2">
          <span className="text-[10px] text-zinc-600 uppercase tracking-wider w-16">
            Action
          </span>
          <span className="text-[13px] font-mono text-teal-400">
            {decision.action}
          </span>
        </div>
        <div className="flex items-center gap-2">
          <span className="text-[10px] text-zinc-600 uppercase tracking-wider w-16">
            Resource
          </span>
          <span className="text-[13px] font-mono text-zinc-300">
            {decision.resource_type}::{decision.resource_id}
          </span>
        </div>
        <div className="flex items-center gap-2">
          <span className="text-[10px] text-zinc-600 uppercase tracking-wider w-16">
            Reason
          </span>
          <span className="text-[13px] text-pink-400">
            {decision.denial_reason}
          </span>
        </div>
        {decision.module_name && (
          <div className="flex items-center gap-2">
            <span className="text-[10px] text-zinc-600 uppercase tracking-wider w-16">
              Module
            </span>
            <span className="text-[13px] font-mono text-zinc-400">
              {decision.module_name}
            </span>
          </div>
        )}
      </div>

      {/* Policy preview */}
      {decision.generated_policy && (
        <div className="mb-3">
          <button
            onClick={() => setShowPolicy(!showPolicy)}
            className="text-[11px] text-teal-400 hover:text-teal-300 transition-colors"
          >
            {showPolicy ? "Hide" : "Show"} generated policy
          </button>
          {showPolicy && (
            <pre className="mt-2 p-2.5 bg-black/30 rounded text-[11px] font-mono text-zinc-400 overflow-x-auto whitespace-pre-wrap border border-white/[0.04]">
              {decision.generated_policy}
            </pre>
          )}
        </div>
      )}

      {/* Actions */}
      <div className="flex items-center gap-3 pt-2 border-t border-white/[0.04]">
        <div className="flex items-center gap-2 flex-1">
          <select
            value={scope}
            onChange={(e) => setScope(e.target.value as PolicyScope)}
            className="bg-[#111115] text-zinc-400 text-xs rounded-sm px-2 py-1.5 focus:outline-none flex-1"
          >
            {(Object.keys(SCOPE_LABELS) as PolicyScope[]).map((s) => (
              <option key={s} value={s}>
                {SCOPE_LABELS[s]}
              </option>
            ))}
          </select>
          <button
            onClick={() => onApprove(decision.id, scope, decision.tenant)}
            disabled={acting}
            className="px-3 py-1.5 bg-teal-500/20 hover:bg-teal-500/30 text-teal-400 text-xs rounded-sm transition-colors disabled:opacity-40 disabled:cursor-not-allowed"
          >
            Allow
          </button>
        </div>
        <button
          onClick={() => onDeny(decision.id, decision.tenant)}
          disabled={acting}
          className="px-3 py-1.5 bg-pink-500/20 hover:bg-pink-500/30 text-pink-400 text-xs rounded-sm transition-colors disabled:opacity-40 disabled:cursor-not-allowed"
        >
          Deny
        </button>
      </div>
    </div>
  );
}

export default function DecisionsPage() {
  const [initialLoading, setInitialLoading] = useState(true);
  const [initialError, setInitialError] = useState<string | null>(null);
  const [tenant, setTenant] = useState<string>(ALL_TENANTS);
  const [tenants, setTenants] = useState<string[]>([]);
  const [statusFilter, setStatusFilter] = useState<string>("all");
  const [acting, setActing] = useState(false);
  const [liveDecisions, setLiveDecisions] = useState<PendingDecision[]>([]);

  const loadInitial = useCallback(async () => {
    setInitialLoading(true);
    setInitialError(null);
    try {
      const specs = await fetchSpecs();
      const tenantSet = new Set<string>();
      for (const s of specs) {
        if (s.tenant && s.tenant !== "temper-system") tenantSet.add(s.tenant);
      }
      setTenants(Array.from(tenantSet).sort());
    } catch (err) {
      setInitialError(
        err instanceof Error ? err.message : "Failed to load decisions",
      );
    } finally {
      setInitialLoading(false);
    }
  }, []);

  useEffect(() => {
    loadInitial();
  }, [loadInitial]);

  const decisionsPoll = usePolling<DecisionsResponse>({
    fetcher: () =>
      tenant === ALL_TENANTS
        ? fetchAllDecisions(
            statusFilter !== "all" ? { status: statusFilter } : undefined,
          )
        : fetchDecisions(
            tenant,
            statusFilter !== "all" ? { status: statusFilter } : undefined,
          ),
    interval: 5000,
    enabled: !initialLoading && !initialError,
  });

  const data = decisionsPoll.data;
  const lastUpdated = useRelativeTime(decisionsPoll.lastUpdated);

  // SSE for live pending decisions
  useEffect(() => {
    if (initialLoading || initialError) return;
    const cleanup =
      tenant === ALL_TENANTS
        ? subscribeAllPendingDecisions((decision) => {
            setLiveDecisions((prev) => [...prev.slice(-49), decision]);
            decisionsPoll.refresh();
          })
        : subscribePendingDecisions(tenant, (decision) => {
            setLiveDecisions((prev) => [...prev.slice(-49), decision]);
            decisionsPoll.refresh();
          });
    return cleanup;
  }, [initialLoading, initialError, tenant]); // eslint-disable-line react-hooks/exhaustive-deps

  const handleApprove = useCallback(
    async (id: string, scope: PolicyScope, decisionTenant: string) => {
      setActing(true);
      try {
        await approveDecision(decisionTenant, id, scope);
        await decisionsPoll.refresh();
      } catch {
        // Error handled by polling
      } finally {
        setActing(false);
      }
    },
    [decisionsPoll],
  );

  const handleDeny = useCallback(
    async (id: string, decisionTenant: string) => {
      setActing(true);
      try {
        await denyDecision(decisionTenant, id);
        await decisionsPoll.refresh();
      } catch {
        // Error handled by polling
      } finally {
        setActing(false);
      }
    },
    [decisionsPoll],
  );

  const pendingDecisions = useMemo(() => {
    if (!data) return [];
    return data.decisions.filter((d) => d.status === "pending");
  }, [data]);

  const resolvedDecisions = useMemo(() => {
    if (!data) return [];
    return data.decisions.filter((d) => d.status !== "pending");
  }, [data]);

  const showTenantBadge = tenant === ALL_TENANTS;

  if (initialLoading) {
    return (
      <div className="animate-pulse">
        <div className="h-6 bg-zinc-800/60 rounded w-36 mb-1.5" />
        <div className="h-3.5 bg-zinc-800/40 rounded w-64 mb-6" />
        <div className="grid grid-cols-4 gap-3 mb-6">
          {[0, 1, 2, 3].map((i) => (
            <div key={i} className="glass rounded p-3.5">
              <div className="h-3 bg-zinc-800/50 rounded w-20 mb-2" />
              <div className="h-8 bg-zinc-800/50 rounded w-10" />
            </div>
          ))}
        </div>
      </div>
    );
  }

  if (initialError) {
    return (
      <ErrorDisplay
        title="Cannot load decisions"
        message={initialError}
        retry={loadInitial}
      />
    );
  }

  return (
    <div className="animate-fade-in">
      {/* Header */}
      <div className="flex items-center justify-between mb-6">
        <div>
          <h1 className="text-2xl font-bold text-zinc-100 tracking-tight font-display">
            Decisions
          </h1>
          <p className="text-sm text-zinc-600 mt-0.5">
            Authorization decisions requiring approval or review
          </p>
        </div>
        <div className="flex items-center gap-3">
          {liveDecisions.length > 0 && (
            <div className="flex items-center gap-1.5">
              <div className="w-1.5 h-1.5 bg-pink-400 rounded-full animate-pulse" />
              <span className="text-xs text-zinc-500 font-mono">
                {liveDecisions.length} live
              </span>
            </div>
          )}
          <select
            value={tenant}
            onChange={(e) => setTenant(e.target.value)}
            className="bg-[#111115] text-zinc-400 text-xs rounded-sm px-2 py-1.5 focus:outline-none"
          >
            <option value={ALL_TENANTS}>All tenants</option>
            {tenants.map((t) => (
              <option key={t} value={t}>{t}</option>
            ))}
          </select>
          <select
            value={statusFilter}
            onChange={(e) => setStatusFilter(e.target.value)}
            className="bg-[#111115] text-zinc-400 text-xs rounded-sm px-2 py-1.5 focus:outline-none"
          >
            <option value="all">All statuses</option>
            <option value="pending">Pending</option>
            <option value="approved">Approved</option>
            <option value="denied">Denied</option>
            <option value="expired">Expired</option>
          </select>
          {lastUpdated && (
            <span className="text-xs text-zinc-600">Updated {lastUpdated}</span>
          )}
        </div>
      </div>

      {/* Stats row */}
      <div className="grid grid-cols-4 gap-3 mb-6">
        <StatCard
          label="Pending"
          value={data?.pending_count ?? 0}
          color={
            data && data.pending_count > 0 ? "text-pink-400" : undefined
          }
        />
        <StatCard
          label="Approved"
          value={data?.approved_count ?? 0}
          color="text-teal-400"
        />
        <StatCard
          label="Denied"
          value={data?.denied_count ?? 0}
          color={
            data && data.denied_count > 0 ? "text-amber-400" : undefined
          }
        />
        <StatCard label="Total" value={data?.total ?? 0} />
      </div>

      {/* Pending Decisions */}
      {pendingDecisions.length > 0 && (
        <div className="mb-6">
          <div className="flex items-center gap-2 mb-3">
            <div className="w-1.5 h-1.5 bg-pink-400 rounded-full animate-pulse" />
            <h2 className="text-base font-semibold text-zinc-200 tracking-tight">
              Pending Decisions
            </h2>
            <span className="text-[10px] font-mono text-zinc-600">
              {pendingDecisions.length}
            </span>
          </div>
          <div className="grid gap-3">
            {pendingDecisions.map((d) => (
              <DecisionCard
                key={d.id}
                decision={d}
                onApprove={handleApprove}
                onDeny={handleDeny}
                acting={acting}
                showTenant={showTenantBadge}
              />
            ))}
          </div>
        </div>
      )}

      {pendingDecisions.length === 0 && statusFilter === "all" && (
        <div className="glass rounded p-6 text-center mb-6">
          <p className="text-sm text-zinc-500">
            No pending decisions. All clear.
          </p>
        </div>
      )}

      {/* History Table */}
      {resolvedDecisions.length > 0 && (
        <div>
          <h2 className="text-base font-semibold text-zinc-200 mb-3 tracking-tight">
            Decision History
          </h2>
          <div className="glass rounded overflow-hidden max-h-96 overflow-y-auto">
            <table className="w-full text-[13px]">
              <thead className="sticky top-0 bg-[#111115]/90 backdrop-blur-sm z-10">
                <tr className="border-b border-white/[0.06]">
                  {showTenantBadge && (
                    <th className="text-left px-3.5 py-2.5 text-zinc-600 font-medium text-xs uppercase tracking-wider">
                      Tenant
                    </th>
                  )}
                  <th className="text-left px-3.5 py-2.5 text-zinc-600 font-medium text-xs uppercase tracking-wider">
                    Agent
                  </th>
                  <th className="text-left px-3.5 py-2.5 text-zinc-600 font-medium text-xs uppercase tracking-wider">
                    Action
                  </th>
                  <th className="text-left px-3.5 py-2.5 text-zinc-600 font-medium text-xs uppercase tracking-wider">
                    Resource
                  </th>
                  <th className="text-left px-3.5 py-2.5 text-zinc-600 font-medium text-xs uppercase tracking-wider">
                    Status
                  </th>
                  <th className="text-left px-3.5 py-2.5 text-zinc-600 font-medium text-xs uppercase tracking-wider">
                    Scope
                  </th>
                  <th className="text-right px-3.5 py-2.5 text-zinc-600 font-medium text-xs uppercase tracking-wider">
                    Decided
                  </th>
                </tr>
              </thead>
              <tbody>
                {resolvedDecisions.map((d, i) => {
                  const decidedTs = d.decided_at
                    ? new Date(d.decided_at).toLocaleString()
                    : "--";
                  return (
                    <tr
                      key={d.id}
                      className={`border-b border-white/[0.03] ${i % 2 === 1 ? "bg-white/[0.01]" : ""}`}
                    >
                      {showTenantBadge && (
                        <td className="px-3.5 py-2.5 font-mono text-zinc-500 text-[11px]">
                          {d.tenant}
                        </td>
                      )}
                      <td className="px-3.5 py-2.5 font-mono text-zinc-300">
                        {d.agent_id}
                      </td>
                      <td className="px-3.5 py-2.5 font-mono text-teal-400">
                        {d.action}
                      </td>
                      <td className="px-3.5 py-2.5 font-mono text-zinc-400">
                        {d.resource_type}::{d.resource_id}
                      </td>
                      <td className="px-3.5 py-2.5">
                        <span
                          className={`text-xs font-mono px-2 py-0.5 rounded-full ${
                            d.status === "approved"
                              ? "bg-teal-500/15 text-teal-400"
                              : d.status === "denied"
                                ? "bg-pink-500/15 text-pink-400"
                                : "bg-amber-500/15 text-amber-400"
                          }`}
                        >
                          {d.status}
                        </span>
                      </td>
                      <td className="px-3.5 py-2.5 font-mono text-zinc-500 text-[11px]">
                        {d.approved_scope ?? "--"}
                      </td>
                      <td className="px-3.5 py-2.5 text-right font-mono text-zinc-600 text-[11px]">
                        {decidedTs}
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        </div>
      )}
    </div>
  );
}
