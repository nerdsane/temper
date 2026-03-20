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
import { useSSERefresh, useRelativeTime } from "@/lib/hooks";
import type {
  DecisionsResponse,
  PendingDecision,
  PolicyScopeMatrix,
  SpecSummary,
} from "@/lib/types";
import ErrorDisplay from "@/components/ErrorDisplay";
import StatCard from "@/components/StatCard";
import PolicyBuilder from "@/components/PolicyBuilder";
import DecisionGroup from "@/components/DecisionGroup";
import BatchApproveBar from "@/components/BatchApproveBar";
import {
  redactSensitiveFields,
  groupByDate,
} from "@/lib/utils";
import { groupDecisions, type GroupingStrategy } from "@/lib/decision-grouping";


const ALL_TENANTS = "__all__";

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
  onApprove: (id: string, matrix: PolicyScopeMatrix, tenant: string) => void;
  onDeny: (id: string, tenant: string) => void;
  acting: boolean;
  showTenant?: boolean;
}) {
  const ts = new Date(decision.created_at);
  const timeStr = ts.toLocaleString();

  const redactedAttrs = redactSensitiveFields(decision.resource_attrs);

  return (
    <div className="glass rounded p-4 animate-fade-in">
      <div className="flex items-start justify-between mb-3">
        <div className="flex items-center gap-2">
          <div className="w-2 h-2 rounded-full bg-[var(--color-accent-pink)] animate-pulse" />
          <span className="text-sm font-mono text-[var(--color-text-primary)] truncate max-w-[200px]" title={decision.agent_id}>
            {decision.agent_id}
          </span>
          {showTenant && (
            <span className="text-[10px] font-mono px-1.5 py-0.5 rounded bg-[var(--color-bg-elevated)] text-[var(--color-text-secondary)]">
              {decision.tenant}
            </span>
          )}
        </div>
        <span className="text-[11px] text-[var(--color-text-muted)] font-mono">{timeStr}</span>
      </div>

      <div className="space-y-1.5 mb-3">
        <div className="flex items-center gap-2">
          <span className="text-[10px] text-[var(--color-text-muted)] uppercase tracking-wider w-16">
            Action
          </span>
          <span className="text-[13px] font-mono text-[var(--color-accent-teal)]">
            {decision.action}
          </span>
        </div>
        <div className="flex items-center gap-2">
          <span className="text-[10px] text-[var(--color-text-muted)] uppercase tracking-wider w-16">
            Resource
          </span>
          <span className="text-[13px] font-mono text-[var(--color-text-secondary)] truncate max-w-[280px] inline-block" title={`${decision.resource_type}::${decision.resource_id}`}>
            {decision.resource_type}::{decision.resource_id}
          </span>
        </div>
        <div className="flex items-center gap-2">
          <span className="text-[10px] text-[var(--color-text-muted)] uppercase tracking-wider w-16">
            Reason
          </span>
          <span className="text-[13px] text-[var(--color-accent-pink)]">
            {redactDenialReason(decision.denial_reason)}
          </span>
        </div>
        {decision.module_name && (
          <div className="flex items-center gap-2">
            <span className="text-[10px] text-[var(--color-text-muted)] uppercase tracking-wider w-16">
              Module
            </span>
            <span className="text-[13px] font-mono text-[var(--color-text-secondary)]">
              {decision.module_name}
            </span>
          </div>
        )}
        {decision.resource_attrs &&
          Object.keys(decision.resource_attrs).length > 0 && (
            <div className="flex items-start gap-2">
              <span className="text-[10px] text-[var(--color-text-muted)] uppercase tracking-wider w-16 pt-0.5">
                Attrs
              </span>
              <pre className="text-[11px] font-mono text-[var(--color-text-secondary)] overflow-x-auto whitespace-pre-wrap">
                {JSON.stringify(redactedAttrs, null, 2)}
              </pre>
            </div>
          )}
      </div>

      {/* Policy Builder replaces old scope dropdown */}
      <div className="pt-2 border-t border-[var(--color-border)]">
        <PolicyBuilder
          decision={decision}
          onApprove={(matrix) => onApprove(decision.id, matrix, decision.tenant)}
          onDeny={() => onDeny(decision.id, decision.tenant)}
          disabled={acting}
        />
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
    ? new Date(decision.decided_at).toLocaleString(undefined, {
        month: "short",
        day: "numeric",
        hour: "2-digit",
        minute: "2-digit",
      })
    : "--";

  return (
    <>
      <tr
        className={`border-b border-[var(--color-border)] cursor-pointer hover:bg-[var(--color-bg-elevated)] transition-colors ${even ? "bg-[var(--color-bg-elevated)]" : ""}`}
        onClick={() =>
          decision.generated_policy && setExpanded((prev) => !prev)
        }
      >
        {showTenant && (
          <td className="px-3 py-2 font-mono text-[var(--color-text-secondary)] text-[11px] whitespace-nowrap">
            {decision.tenant}
          </td>
        )}
        <td className="px-3 py-2 font-mono text-[var(--color-text-secondary)] max-w-[140px] truncate" title={decision.agent_id}>
          {decision.agent_id}
        </td>
        <td className="px-3 py-2 font-mono text-[var(--color-accent-teal)] max-w-[100px] truncate" title={decision.action}>
          {decision.action}
        </td>
        <td className="px-3 py-2 font-mono text-[var(--color-text-secondary)] max-w-[160px] truncate" title={`${decision.resource_type}::${decision.resource_id}`}>
          {decision.resource_type}::{decision.resource_id}
        </td>
        <td className="px-3 py-2 whitespace-nowrap">
          <span
            className={`text-[11px] font-mono px-1.5 py-0.5 rounded-full ${
              decision.status === "approved"
                ? "bg-[var(--color-accent-teal-dim)] text-[var(--color-accent-teal)]"
                : decision.status === "denied"
                  ? "bg-[var(--color-accent-pink-dim)] text-[var(--color-accent-pink)]"
                  : "bg-[var(--color-accent-pink-dim)] text-[var(--color-accent-pink)]"
            }`}
          >
            {decision.status}
          </span>
        </td>
        <td className="px-3 py-2 font-mono text-[var(--color-text-secondary)] text-[11px] max-w-[180px] truncate" title={decision.approved_scope ? `${decision.approved_scope.principal} / ${decision.approved_scope.action} / ${decision.approved_scope.resource}` : undefined}>
          {decision.approved_scope ? `${decision.approved_scope.principal} / ${decision.approved_scope.action} / ${decision.approved_scope.resource}` : "--"}
        </td>
        <td className="px-3 py-2 text-right font-mono text-[var(--color-text-muted)] text-[11px] whitespace-nowrap">
          {decidedTs}
          {decision.generated_policy && (
            <span className="ml-1 text-[var(--color-text-muted)]">
              {expanded ? "\u25B4" : "\u25BE"}
            </span>
          )}
        </td>
      </tr>
      {expanded && decision.generated_policy && (
        <tr className="border-b border-[var(--color-border)]">
          <td
            colSpan={showTenant ? 7 : 6}
            className="px-3.5 py-2.5"
          >
            <pre className="p-2.5 bg-black/30 rounded text-[11px] font-mono text-[var(--color-text-secondary)] overflow-x-auto whitespace-pre-wrap border border-[var(--color-border)]">
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
  const [batchMode, setBatchMode] = useState(false);
  const [selectedIds, setSelectedIds] = useState<Set<string>>(new Set());
  const [groupingStrategy, setGroupingStrategy] = useState<GroupingStrategy>("action_resource");

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

  const decisionsPoll = useSSERefresh<DecisionsResponse>({
    fetcher: () =>
      tenant === ALL_TENANTS
        ? fetchAllDecisions(
            statusFilter !== "all" ? { status: statusFilter } : undefined,
          )
        : fetchDecisions(
            tenant,
            statusFilter !== "all" ? { status: statusFilter } : undefined,
          ),
    sseKinds: ["Decisions"],
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
    async (id: string, matrix: PolicyScopeMatrix, decisionTenant: string) => {
      setActingIds((prev) => new Set(prev).add(id));
      setActionError(null);
      try {
        await approveDecision(decisionTenant, id, matrix);
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

  const pendingGroups = useMemo(
    () => groupDecisions(pendingDecisions, groupingStrategy),
    [pendingDecisions, groupingStrategy],
  );

  const selectedDecisions = useMemo(
    () => pendingDecisions.filter((d) => selectedIds.has(d.id)),
    [pendingDecisions, selectedIds],
  );

  const handleToggleSelect = useCallback((id: string) => {
    setSelectedIds((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  }, []);

  const handleToggleGroup = useCallback((ids: string[]) => {
    setSelectedIds((prev) => {
      const next = new Set(prev);
      const allSelected = ids.every((id) => next.has(id));
      if (allSelected) {
        for (const id of ids) next.delete(id);
      } else {
        for (const id of ids) next.add(id);
      }
      return next;
    });
  }, []);

  const handleBatchApprove = useCallback(
    async (ids: string[], matrix: PolicyScopeMatrix) => {
      setActionError(null);
      for (const id of ids) {
        setActingIds((prev) => new Set(prev).add(id));
      }
      const allDecisions = data?.decisions || [];
      const results = await Promise.allSettled(
        ids.map((id) => {
          const decision = allDecisions.find((d) => d.id === id);
          return approveDecision(decision?.tenant || "", id, matrix);
        }),
      );
      const succeeded = results.filter((r) => r.status === "fulfilled").length;
      const failed = results.filter((r) => r.status === "rejected").length;
      if (failed > 0) {
        const firstError = results.find((r) => r.status === "rejected") as PromiseRejectedResult;
        setActionError(`${failed} approval(s) failed: ${firstError.reason}`);
      }
      setSelectedIds(new Set());
      for (const id of ids) {
        setActingIds((prev) => {
          const next = new Set(prev);
          next.delete(id);
          return next;
        });
      }
      await decisionsPoll.refresh();
      return { succeeded, failed };
    },
    [decisionsPoll, data],
  );

  const groupedHistory = useMemo(
    () => groupByDate(resolvedDecisions, (d) => d.decided_at),
    [resolvedDecisions],
  );

  const showTenantBadge = tenant === ALL_TENANTS;

  if (initialLoading) {
    return (
      <div className="animate-pulse">
        <div className="h-6 bg-[var(--color-border)] rounded w-36 mb-1.5" />
        <div className="h-3.5 bg-[var(--color-border)] rounded w-64 mb-6" />
        <div className="grid grid-cols-4 gap-3 mb-6">
          {[0, 1, 2, 3].map((i) => (
            <div key={i} className="glass rounded-[2px] p-4">
              <div className="h-3 bg-[var(--color-border)] rounded w-20 mb-2" />
              <div className="h-8 bg-[var(--color-border)] rounded w-10" />
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
          <h1 className="text-2xl text-[var(--color-text-primary)] tracking-tight font-serif">
            Decisions
          </h1>
          <p className="text-sm text-[var(--color-text-muted)] mt-0.5">
            Authorization decisions requiring approval or review
          </p>
        </div>
        <div className="flex items-center gap-3">
          {liveDecisions.length > 0 && (
            <div className="flex items-center gap-1.5">
              <div className="w-1.5 h-1.5 bg-[var(--color-accent-pink)] rounded-full animate-pulse" />
              <span className="text-xs text-[var(--color-text-secondary)] font-mono">
                {liveDecisions.length} live
              </span>
            </div>
          )}
          <select
            value={tenant}
            onChange={(e) => setTenant(e.target.value)}
            className="bg-[var(--color-bg-surface)] text-[var(--color-text-secondary)] text-xs rounded-sm px-2 py-1.5 focus:outline-none"
          >
            <option value={ALL_TENANTS}>All tenants</option>
            {tenants.map((t) => (
              <option key={t} value={t}>{t}</option>
            ))}
          </select>
          <select
            value={statusFilter}
            onChange={(e) => setStatusFilter(e.target.value)}
            className="bg-[var(--color-bg-surface)] text-[var(--color-text-secondary)] text-xs rounded-sm px-2 py-1.5 focus:outline-none"
          >
            <option value="all">All statuses</option>
            <option value="pending">Pending</option>
            <option value="approved">Approved</option>
            <option value="denied">Denied</option>
            <option value="expired">Expired</option>
          </select>
          {pendingDecisions.length > 1 && (
            <>
              <button
                onClick={() => {
                  setBatchMode(!batchMode);
                  setSelectedIds(new Set());
                }}
                className={`px-2.5 py-1.5 text-xs rounded-sm transition-colors ${
                  batchMode
                    ? "bg-[var(--color-accent-teal-dim)] text-[var(--color-accent-teal)] ring-1 ring-[var(--color-accent-teal)]"
                    : "bg-[var(--color-bg-elevated)] text-[var(--color-text-secondary)] hover:bg-[var(--color-border-hover)]"
                }`}
              >
                Batch
              </button>
              {batchMode && (
                <select
                  value={groupingStrategy}
                  onChange={(e) => setGroupingStrategy(e.target.value as GroupingStrategy)}
                  className="bg-[var(--color-bg-surface)] text-[var(--color-text-secondary)] text-xs rounded-sm px-2 py-1.5 focus:outline-none"
                >
                  <option value="action_resource">By action + type</option>
                  <option value="agent_action">By agent + action</option>
                  <option value="agent_type_action">By agent type + action</option>
                </select>
              )}
            </>
          )}
          {resolvedDecisions.length > 0 && (
            <button
              onClick={() => exportDecisions(data?.decisions ?? [])}
              className="px-2.5 py-1.5 bg-[var(--color-bg-elevated)] hover:bg-[var(--color-border-hover)] text-[var(--color-text-secondary)] text-xs rounded-sm transition-colors"
            >
              Export
            </button>
          )}
          {lastUpdated && (
            <span className="text-xs text-[var(--color-text-muted)]">Updated {lastUpdated}</span>
          )}
        </div>
      </div>

      {/* Stats row */}
      <div className="grid grid-cols-4 gap-3 mb-6">
        <StatCard
          label="Pending"
          value={data?.pending_count ?? 0}
          color={
            data && data.pending_count > 0 ? "text-[var(--color-accent-pink)]" : undefined
          }
        />
        <StatCard
          label="Approved"
          value={data?.approved_count ?? 0}
          color="text-[var(--color-accent-teal)]"
        />
        <StatCard
          label="Denied"
          value={data?.denied_count ?? 0}
          color={
            data && data.denied_count > 0 ? "text-[var(--color-accent-pink)]" : undefined
          }
        />
        <StatCard label="Total" value={data?.total ?? 0} />
      </div>

      {/* Polling error banner */}
      {decisionsPoll.error && !data && (
        <div role="alert" className="mb-4 flex items-center justify-between gap-2 rounded bg-[var(--color-accent-pink-dim)] border border-[var(--color-accent-pink)]/20 px-4 py-2.5">
          <p className="text-sm text-[var(--color-accent-pink)]">Failed to load decisions: {decisionsPoll.error}</p>
          <button onClick={() => decisionsPoll.refresh()} className="text-[var(--color-accent-teal)] hover:text-[var(--color-accent-teal)] text-xs flex-shrink-0">Retry</button>
        </div>
      )}

      {/* Action error banner */}
      {actionError && (
        <div role="alert" className="mb-4 flex items-center justify-between gap-2 rounded bg-[var(--color-accent-pink-dim)] border border-[var(--color-accent-pink)]/20 px-4 py-2.5">
          <p className="text-sm text-[var(--color-accent-pink)]">{actionError}</p>
          <button onClick={() => setActionError(null)} className="text-[var(--color-accent-pink)] hover:text-[var(--color-accent-pink)] text-xs flex-shrink-0" aria-label="Dismiss error">Dismiss</button>
        </div>
      )}

      {/* Pending Decisions */}
      {pendingDecisions.length > 0 && (
        <div className={`mb-6 ${batchMode && selectedDecisions.length > 0 ? "pb-24" : ""}`}>
          <div className="flex items-center gap-2 mb-3">
            <div className="w-1.5 h-1.5 bg-[var(--color-accent-pink)] rounded-full animate-pulse" />
            <h2 className="text-base font-semibold text-[var(--color-text-primary)] tracking-tight">
              Pending Decisions
            </h2>
            <span className="text-[10px] font-mono text-[var(--color-text-muted)]">
              {pendingDecisions.length}
            </span>
          </div>

          {batchMode ? (
            <div className="grid gap-3">
              {Array.from(pendingGroups.entries()).map(([key, decisions]) => (
                <DecisionGroup
                  key={key}
                  groupKey={key}
                  strategy={groupingStrategy}
                  decisions={decisions}
                  selectedIds={selectedIds}
                  onToggleSelect={handleToggleSelect}
                  onToggleGroup={handleToggleGroup}
                />
              ))}
            </div>
          ) : (
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
          )}
        </div>
      )}

      {pendingDecisions.length === 0 && statusFilter === "all" && (
        <div className="glass rounded-[2px] p-6 text-center mb-6">
          <p className="text-sm text-[var(--color-text-secondary)]">
            No pending decisions. All clear.
          </p>
        </div>
      )}

      {/* Batch approve bar */}
      {batchMode && (
        <BatchApproveBar
          selectedDecisions={selectedDecisions}
          onApprove={handleBatchApprove}
          onClear={() => setSelectedIds(new Set())}
        />
      )}

      {/* History Table — grouped by date */}
      {resolvedDecisions.length > 0 && (
        <div>
          <h2 className="text-base font-semibold text-[var(--color-text-primary)] mb-3 tracking-tight">
            Decision History
          </h2>
          {Array.from(groupedHistory.entries()).map(
            ([bucket, decisions]) => (
              <div key={bucket} className="mb-4">
                <div className="flex items-center gap-2 mb-2">
                  <span className="text-[11px] font-medium text-[var(--color-text-secondary)] uppercase tracking-wider">
                    {bucket}
                  </span>
                  <div className="flex-1 h-px bg-[var(--color-bg-elevated)]" />
                  <span className="text-[10px] font-mono text-[var(--color-text-muted)]">
                    {decisions.length}
                  </span>
                </div>
                <div className="glass rounded overflow-hidden max-h-96 overflow-y-auto overflow-x-auto">
                  <table className="w-full text-[13px]">
                    <thead className="sticky top-0 bg-[color-mix(in_srgb,var(--color-bg-surface)_90%,transparent)] backdrop-blur-sm z-10">
                      <tr className="border-b border-[var(--color-border)]">
                        {showTenantBadge && (
                          <th className="text-left px-3 py-2 text-[var(--color-text-muted)] font-medium text-xs uppercase tracking-wider whitespace-nowrap">
                            Tenant
                          </th>
                        )}
                        <th className="text-left px-3 py-2 text-[var(--color-text-muted)] font-medium text-xs uppercase tracking-wider whitespace-nowrap">
                          Agent
                        </th>
                        <th className="text-left px-3 py-2 text-[var(--color-text-muted)] font-medium text-xs uppercase tracking-wider whitespace-nowrap">
                          Action
                        </th>
                        <th className="text-left px-3 py-2 text-[var(--color-text-muted)] font-medium text-xs uppercase tracking-wider whitespace-nowrap">
                          Resource
                        </th>
                        <th className="text-left px-3 py-2 text-[var(--color-text-muted)] font-medium text-xs uppercase tracking-wider whitespace-nowrap">
                          Status
                        </th>
                        <th className="text-left px-3 py-2 text-[var(--color-text-muted)] font-medium text-xs uppercase tracking-wider whitespace-nowrap">
                          Scope
                        </th>
                        <th className="text-right px-3 py-2 text-[var(--color-text-muted)] font-medium text-xs uppercase tracking-wider whitespace-nowrap">
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
