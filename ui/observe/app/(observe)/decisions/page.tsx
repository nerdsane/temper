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
import StatCard from "@/components/StatCard";
import {
  redactSensitiveFields,
  generatePolicyPreview,
  groupByDate,
} from "@/lib/utils";


const ALL_TENANTS = "__all__";


const SCOPE_LABELS: Record<PolicyScope, string> = {
  narrow: "Narrow -- exact resource only",
  medium: "Medium -- same resource type",
  broad: "Broad -- all resources for action",
};

/** Redact inline secrets in denial reason strings (e.g. "token=sk-abc123"). */
function redactDenialReason(reason: string): string {
  return reason.replace(
    /\b(authorization|api_key|apikey|token|secret|password|cookie|credential|bearer|jwt|session_token|access_token|refresh_token|private_key)\s*[=:]\s*\S+/gi,
    (match) => {
      const sep = match.includes("=") ? "=" : ":";
      const key = match.split(/[=:]/)[0].trim();
      return `${key}${sep}[redacted]`;
    },
  );
}

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
  const [showPreview, setShowPreview] = useState(false);
  const ts = new Date(decision.created_at);
  const timeStr = ts.toLocaleString();

  const policyPreview = generatePolicyPreview(
    decision.agent_id,
    decision.action,
    decision.resource_type,
    decision.resource_id,
    scope,
  );

  const redactedAttrs = redactSensitiveFields(decision.resource_attrs);

  return (
    <div className="glass rounded p-4 animate-fade-in">
      <div className="flex items-start justify-between mb-3">
        <div className="flex items-center gap-2">
          <div className="w-2 h-2 rounded-full bg-pink-400 animate-pulse" />
          <span className="text-sm font-mono text-zinc-200 truncate max-w-[200px]" title={decision.agent_id}>
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
          <span className="text-[13px] font-mono text-zinc-300 truncate max-w-[280px] inline-block" title={`${decision.resource_type}::${decision.resource_id}`}>
            {decision.resource_type}::{decision.resource_id}
          </span>
        </div>
        <div className="flex items-center gap-2">
          <span className="text-[10px] text-zinc-600 uppercase tracking-wider w-16">
            Reason
          </span>
          <span className="text-[13px] text-pink-400">
            {redactDenialReason(decision.denial_reason)}
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
        {decision.resource_attrs &&
          Object.keys(decision.resource_attrs).length > 0 && (
            <div className="flex items-start gap-2">
              <span className="text-[10px] text-zinc-600 uppercase tracking-wider w-16 pt-0.5">
                Attrs
              </span>
              <pre className="text-[11px] font-mono text-zinc-500 overflow-x-auto whitespace-pre-wrap">
                {JSON.stringify(redactedAttrs, null, 2)}
              </pre>
            </div>
          )}
      </div>

      {/* Live policy preview */}
      <div className="mb-3">
        <button
          onClick={() => setShowPreview(!showPreview)}
          className="text-[11px] text-teal-400 hover:text-teal-300 transition-colors"
        >
          {showPreview ? "Hide" : "Preview"} policy
        </button>
        {showPreview && (
          <pre className="mt-2 p-2.5 bg-black/30 rounded text-[11px] font-mono text-zinc-400 overflow-x-auto whitespace-pre-wrap border border-white/[0.04]">
            {policyPreview}
          </pre>
        )}
      </div>

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

function HistoryRow({
  decision,
  showTenant,
  even,
}: {
  decision: PendingDecision;
  showTenant: boolean;
  even: boolean;
}) {
  const [expanded, setExpanded] = useState(false);
  const decidedTs = decision.decided_at
    ? new Date(decision.decided_at).toLocaleString()
    : "--";

  return (
    <>
      <tr
        className={`border-b border-white/[0.03] cursor-pointer hover:bg-white/[0.03] transition-colors ${even ? "bg-white/[0.01]" : ""}`}
        onClick={() =>
          decision.generated_policy && setExpanded((prev) => !prev)
        }
      >
        {showTenant && (
          <td className="px-3.5 py-2.5 font-mono text-zinc-500 text-[11px]">
            {decision.tenant}
          </td>
        )}
        <td className="px-3.5 py-2.5 font-mono text-zinc-300 max-w-[160px] truncate" title={decision.agent_id}>
          {decision.agent_id}
        </td>
        <td className="px-3.5 py-2.5 font-mono text-teal-400 max-w-[120px] truncate" title={decision.action}>
          {decision.action}
        </td>
        <td className="px-3.5 py-2.5 font-mono text-zinc-400 max-w-[200px] truncate" title={`${decision.resource_type}::${decision.resource_id}`}>
          {decision.resource_type}::{decision.resource_id}
        </td>
        <td className="px-3.5 py-2.5">
          <span
            className={`text-xs font-mono px-2 py-0.5 rounded-full ${
              decision.status === "approved"
                ? "bg-teal-500/15 text-teal-400"
                : decision.status === "denied"
                  ? "bg-pink-500/15 text-pink-400"
                  : "bg-amber-500/15 text-amber-400"
            }`}
          >
            {decision.status}
          </span>
        </td>
        <td className="px-3.5 py-2.5 font-mono text-zinc-500 text-[11px]">
          {decision.approved_scope ?? "--"}
        </td>
        <td className="px-3.5 py-2.5 text-right font-mono text-zinc-600 text-[11px]">
          {decidedTs}
          {decision.generated_policy && (
            <span className="ml-1.5 text-zinc-700">
              {expanded ? "\u25B4" : "\u25BE"}
            </span>
          )}
        </td>
      </tr>
      {expanded && decision.generated_policy && (
        <tr className="border-b border-white/[0.03]">
          <td
            colSpan={showTenant ? 7 : 6}
            className="px-3.5 py-2.5"
          >
            <pre className="p-2.5 bg-black/30 rounded text-[11px] font-mono text-zinc-400 overflow-x-auto whitespace-pre-wrap border border-white/[0.04]">
              {decision.generated_policy}
            </pre>
          </td>
        </tr>
      )}
    </>
  );
}

function exportDecisions(decisions: PendingDecision[]) {
  const dateStr = new Date().toISOString().slice(0, 10);
  const blob = new Blob([JSON.stringify(decisions, null, 2)], {
    type: "application/json",
  });
  const url = URL.createObjectURL(blob);
  const a = document.createElement("a");
  a.href = url;
  a.download = `temper-decisions-${dateStr}.json`;
  a.click();
  URL.revokeObjectURL(url);
}

export default function DecisionsPage() {
  const [initialLoading, setInitialLoading] = useState(true);
  const [initialError, setInitialError] = useState<string | null>(null);
  const [tenant, setTenant] = useState<string>(ALL_TENANTS);
  const [tenants, setTenants] = useState<string[]>([]);
  const [statusFilter, setStatusFilter] = useState<string>("all");
  const [actingIds, setActingIds] = useState<Set<string>>(new Set());
  const [actionError, setActionError] = useState<string | null>(null);
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

  // SSE for live pending decisions (best-effort — may fail without admin headers)
  useEffect(() => {
    if (initialLoading || initialError) return;
    const cleanup =
      tenant === ALL_TENANTS
        ? subscribeAllPendingDecisions((decision) => {
            setLiveDecisions((prev) => [...prev.slice(-49), decision]);
            decisionsPoll.refresh().then(() => setLiveDecisions([]));
          })
        : subscribePendingDecisions(tenant, (decision) => {
            setLiveDecisions((prev) => [...prev.slice(-49), decision]);
            decisionsPoll.refresh().then(() => setLiveDecisions([]));
          });
    return cleanup;
  }, [initialLoading, initialError, tenant]); // eslint-disable-line react-hooks/exhaustive-deps

  const handleApprove = useCallback(
    async (id: string, scope: PolicyScope, decisionTenant: string) => {
      setActingIds((prev) => new Set(prev).add(id));
      setActionError(null);
      try {
        await approveDecision(decisionTenant, id, scope);
        await decisionsPoll.refresh();
      } catch (err) {
        const msg = err instanceof Error ? err.message : "Failed to approve decision";
        setActionError(msg);
      } finally {
        setActingIds((prev) => {
          const next = new Set(prev);
          next.delete(id);
          return next;
        });
      }
    },
    [decisionsPoll],
  );

  const handleDeny = useCallback(
    async (id: string, decisionTenant: string) => {
      setActingIds((prev) => new Set(prev).add(id));
      setActionError(null);
      try {
        await denyDecision(decisionTenant, id);
        await decisionsPoll.refresh();
      } catch (err) {
        const msg = err instanceof Error ? err.message : "Failed to deny decision";
        setActionError(msg);
      } finally {
        setActingIds((prev) => {
          const next = new Set(prev);
          next.delete(id);
          return next;
        });
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

  const groupedHistory = useMemo(
    () => groupByDate(resolvedDecisions, (d) => d.decided_at),
    [resolvedDecisions],
  );

  const showTenantBadge = tenant === ALL_TENANTS;

  if (initialLoading) {
    return (
      <div className="animate-pulse">
        <div className="h-6 bg-zinc-800/60 rounded w-36 mb-1.5" />
        <div className="h-3.5 bg-zinc-800/40 rounded w-64 mb-6" />
        <div className="grid grid-cols-4 gap-3 mb-6">
          {[0, 1, 2, 3].map((i) => (
            <div key={i} className="glass rounded-lg p-4">
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
          {resolvedDecisions.length > 0 && (
            <button
              onClick={() => exportDecisions(data?.decisions ?? [])}
              className="px-2.5 py-1.5 bg-zinc-800 hover:bg-zinc-700 text-zinc-400 text-xs rounded-sm transition-colors"
            >
              Export
            </button>
          )}
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

      {/* Action error banner */}
      {actionError && (
        <div role="alert" className="mb-4 flex items-center justify-between gap-2 rounded bg-pink-500/10 border border-pink-500/20 px-4 py-2.5">
          <p className="text-sm text-pink-400">{actionError}</p>
          <button onClick={() => setActionError(null)} className="text-pink-400 hover:text-pink-300 text-xs flex-shrink-0" aria-label="Dismiss error">Dismiss</button>
        </div>
      )}

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
                acting={actingIds.has(d.id)}
                showTenant={showTenantBadge}
              />
            ))}
          </div>
        </div>
      )}

      {pendingDecisions.length === 0 && statusFilter === "all" && (
        <div className="glass rounded-lg p-6 text-center mb-6">
          <p className="text-sm text-zinc-500">
            No pending decisions. All clear.
          </p>
        </div>
      )}

      {/* History Table — grouped by date */}
      {resolvedDecisions.length > 0 && (
        <div>
          <h2 className="text-base font-semibold text-zinc-200 mb-3 tracking-tight">
            Decision History
          </h2>
          {Array.from(groupedHistory.entries()).map(
            ([bucket, decisions]) => (
              <div key={bucket} className="mb-4">
                <div className="flex items-center gap-2 mb-2">
                  <span className="text-[11px] font-medium text-zinc-500 uppercase tracking-wider">
                    {bucket}
                  </span>
                  <div className="flex-1 h-px bg-white/[0.04]" />
                  <span className="text-[10px] font-mono text-zinc-700">
                    {decisions.length}
                  </span>
                </div>
                <div className="glass rounded overflow-hidden max-h-96 overflow-y-auto overflow-x-auto">
                  <table className="w-full text-[13px] table-fixed">
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
                      {decisions.map((d, i) => (
                        <HistoryRow
                          key={d.id}
                          decision={d}
                          showTenant={showTenantBadge}
                          even={i % 2 === 1}
                        />
                      ))}
                    </tbody>
                  </table>
                </div>
              </div>
            ),
          )}
        </div>
      )}
    </div>
  );
}
