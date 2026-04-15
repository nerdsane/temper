"use client";

import { useState, useMemo, useCallback, useEffect } from "react";
import { useParams, useSearchParams } from "next/navigation";
import Link from "next/link";
import { fetchAgentHistory, fetchAllPolicies, fetchSpecs, createPolicy, togglePolicy, deletePolicy } from "@/lib/api";
import { useSSERefresh, useRelativeTime } from "@/lib/hooks";
import type { AgentHistoryResponse, PolicyEntry, SpecSummary } from "@/lib/types";
import ErrorDisplay from "@/components/ErrorDisplay";
import StatCard from "@/components/StatCard";
import VisualPolicyCreator from "@/components/VisualPolicyCreator";

type Tab = "history" | "policies";

export default function AgentDetailPage() {
  const params = useParams();
  const searchParams = useSearchParams();
  const agentId = decodeURIComponent(params.id as string);
  const initialTab = (searchParams.get("tab") as Tab) || "history";

  const [activeTab, setActiveTab] = useState<Tab>(initialTab);
  const [entityTypeFilter, setEntityTypeFilter] = useState<string>("all");
  const [showPolicyCreator, setShowPolicyCreator] = useState(false);
  const [specs, setSpecs] = useState<SpecSummary[]>([]);
  const [actionError, setActionError] = useState<string | null>(null);

  // Fetch specs for policy creator
  useEffect(() => {
    fetchSpecs().then(setSpecs).catch(() => {});
  }, []);

  const tenants = useMemo(() => {
    const set = new Set<string>();
    for (const s of specs) {
      if (s.tenant && s.tenant !== "temper-system") set.add(s.tenant);
    }
    return Array.from(set).sort();
  }, [specs]);

  const historyPoll = useSSERefresh<AgentHistoryResponse>({
    fetcher: () =>
      fetchAgentHistory(agentId, {
        entity_type: entityTypeFilter !== "all" ? entityTypeFilter : undefined,
        limit: 200,
      }),
    sseKinds: ["Agents"],
    enabled: true,
  });

  const data = historyPoll.data;
  const lastUpdated = useRelativeTime(historyPoll.lastUpdated);

  // Fetch policies relevant to this agent
  const policiesPoll = useSSERefresh<{ policies: PolicyEntry[] }>({
    fetcher: async () => {
      const res = await fetchAllPolicies();
      // Filter to policies that mention this agent ID in cedar text
      const relevant = res.policies.filter(
        (p) => p.cedar_text.includes(`"${agentId}"`) || p.cedar_text.includes("principal is Agent"),
      );
      return { policies: relevant };
    },
    sseKinds: ["Policies"],
    enabled: activeTab === "policies",
  });

  const agentPolicies = policiesPoll.data?.policies || [];

  const entityTypes = useMemo(() => {
    if (!data) return [];
    const set = new Set<string>();
    for (const entry of data.history) {
      if (entry.entity_type) set.add(entry.entity_type);
    }
    return Array.from(set).sort();
  }, [data]);

  const stats = useMemo(() => {
    if (!data) return { total: 0, success: 0, errors: 0, denials: 0 };
    let success = 0;
    let errors = 0;
    let denials = 0;
    for (const entry of data.history) {
      if (entry.authz_denied) denials++;
      else if (entry.success) success++;
      else errors++;
    }
    return { total: data.total, success, errors, denials };
  }, [data]);

  const handleTogglePolicy = useCallback(
    async (policy: PolicyEntry) => {
      setActionError(null);
      try {
        await togglePolicy(policy.tenant, policy.policy_id, !policy.enabled);
        await policiesPoll.refresh();
      } catch (err) {
        setActionError(err instanceof Error ? err.message : "Failed to toggle policy");
      }
    },
    [policiesPoll],
  );

  const handleDeletePolicy = useCallback(
    async (policy: PolicyEntry) => {
      setActionError(null);
      try {
        await deletePolicy(policy.tenant, policy.policy_id);
        await policiesPoll.refresh();
      } catch (err) {
        setActionError(err instanceof Error ? err.message : "Failed to delete policy");
      }
    },
    [policiesPoll],
  );

  const handlePolicyCreated = useCallback(
    async (tenant: string, policyId: string, cedarText: string) => {
      await createPolicy(tenant, policyId, cedarText);
      setShowPolicyCreator(false);
      await policiesPoll.refresh();
    },
    [policiesPoll],
  );

  if (historyPoll.error && !data) {
    return (
      <ErrorDisplay
        title={`Cannot load agent ${agentId}`}
        message={historyPoll.error}
        retry={() => historyPoll.refresh()}
        backHref="/agents"
        backLabel="Back to Agents"
      />
    );
  }

  if (historyPoll.loading && !data) {
    return (
      <div className="animate-pulse">
        <div className="h-4 bg-[var(--color-border)] rounded w-24 mb-3" />
        <div className="h-6 bg-[var(--color-border)] rounded w-48 mb-1.5" />
        <div className="h-3.5 bg-[var(--color-border)] rounded w-64 mb-6" />
        <div className="grid grid-cols-4 gap-3 mb-6">
          {[0, 1, 2, 3].map((i) => (
            <div key={i} className="glass rounded p-3.5">
              <div className="h-3 bg-[var(--color-border)] rounded w-20 mb-2" />
              <div className="h-8 bg-[var(--color-border)] rounded w-10" />
            </div>
          ))}
        </div>
      </div>
    );
  }

  return (
    <div className="animate-fade-in">
      {/* Breadcrumb */}
      <div className="mb-4">
        <Link
          href="/agents"
          className="text-[13px] text-[var(--color-text-muted)] hover:text-[var(--color-text-secondary)] transition-colors"
        >
          Agents
        </Link>
        <span className="text-[var(--color-text-muted)] mx-1.5">/</span>
        <span className="text-[13px] text-[var(--color-text-secondary)] font-mono">{agentId}</span>
      </div>

      {/* Header */}
      <div className="flex items-center justify-between mb-6">
        <div>
          <h1 className="text-2xl text-[var(--color-text-primary)] tracking-tight font-serif font-mono">
            {agentId}
          </h1>
          <p className="text-sm text-[var(--color-text-muted)] mt-0.5">
            Action history and authorization policies
          </p>
        </div>
        <div className="flex items-center gap-3">
          {activeTab === "history" && entityTypes.length > 1 && (
            <select
              value={entityTypeFilter}
              onChange={(e) => setEntityTypeFilter(e.target.value)}
              className="bg-[var(--color-bg-surface)] text-[var(--color-text-secondary)] text-xs rounded-sm px-2 py-1.5 focus:outline-none"
            >
              <option value="all">All types</option>
              {entityTypes.map((t) => (
                <option key={t} value={t}>
                  {t}
                </option>
              ))}
            </select>
          )}
          {lastUpdated && (
            <span className="text-xs text-[var(--color-text-muted)]">
              Updated {lastUpdated}
            </span>
          )}
        </div>
      </div>

      {/* Stats row */}
      <div className="grid grid-cols-4 gap-3 mb-6">
        <StatCard label="Total Actions" value={stats.total} />
        <StatCard
          label="Successful"
          value={stats.success}
          color="text-[var(--color-accent-teal)]"
        />
        <StatCard
          label="Errors"
          value={stats.errors}
          color={stats.errors > 0 ? "text-[var(--color-accent-pink)]" : undefined}
        />
        <StatCard
          label="Denials"
          value={stats.denials}
          color={stats.denials > 0 ? "text-[var(--color-accent-pink)]" : undefined}
        />
      </div>

      {/* Tab bar */}
      <div className="flex gap-1 mb-4 border-b border-[var(--color-border)]">
        {(["history", "policies"] as Tab[]).map((tab) => (
          <button
            key={tab}
            type="button"
            onClick={() => setActiveTab(tab)}
            className={`px-3 py-2 text-xs font-medium transition-colors border-b-2 -mb-px ${
              activeTab === tab
                ? "border-[var(--color-accent-teal)] text-[var(--color-accent-teal)]"
                : "border-transparent text-[var(--color-text-muted)] hover:text-[var(--color-text-secondary)]"
            }`}
          >
            {tab === "history" ? "Action History" : `Policies (${agentPolicies.length})`}
          </button>
        ))}
      </div>

      {/* Action error */}
      {actionError && (
        <div className="mb-4 flex items-center justify-between gap-2 rounded bg-[var(--color-accent-pink-dim)] border border-[var(--color-accent-pink)]/20 px-4 py-2.5">
          <p className="text-sm text-[var(--color-accent-pink)]">{actionError}</p>
          <button
            onClick={() => setActionError(null)}
            className="text-[var(--color-accent-pink)] text-xs flex-shrink-0"
          >
            Dismiss
          </button>
        </div>
      )}

      {/* History tab */}
      {activeTab === "history" && (
        <>
          {data && data.history.length > 0 ? (
            <div>
              <div className="glass rounded overflow-hidden max-h-[600px] overflow-y-auto">
                {data.history.map((entry, i) => {
                  const ts = new Date(entry.timestamp);
                  const timeStr = ts.toLocaleString();
                  const isDenial = entry.authz_denied;
                  const isError = !entry.success && !entry.authz_denied;

                  return (
                    <div
                      key={`${entry.timestamp}-${i}`}
                      className={`flex items-center gap-3 px-3.5 py-2.5 border-b border-[var(--color-border)] last:border-b-0 ${
                        isDenial
                          ? "bg-[var(--color-accent-pink-dim)]"
                          : isError
                            ? "bg-[var(--color-accent-pink-dim)]"
                            : ""
                      }`}
                    >
                      <div
                        className={`w-1.5 h-1.5 rounded-full flex-shrink-0 ${
                          isDenial
                            ? "bg-[var(--color-accent-pink)]"
                            : isError
                              ? "bg-[var(--color-accent-pink)]"
                              : "bg-[var(--color-accent-teal)]"
                        }`}
                      />
                      <span className="text-[11px] text-[var(--color-text-muted)] font-mono flex-shrink-0 w-36">
                        {timeStr}
                      </span>
                      <span className="font-mono text-[11px] text-[var(--color-accent-teal)] flex-shrink-0 w-28">
                        {entry.action}
                      </span>
                      <span className="text-[10px] font-mono text-[var(--color-text-secondary)] flex-shrink-0 w-24">
                        {entry.entity_type}
                      </span>
                      <span className="text-[10px] font-mono text-[var(--color-text-muted)] flex-shrink-0 w-20 truncate">
                        {entry.entity_id}
                      </span>
                      {entry.from_status && entry.to_status && (
                        <span className="text-[10px] font-mono text-[var(--color-text-muted)] flex-shrink-0">
                          {entry.from_status}{" "}
                          <span className="text-[var(--color-text-muted)]">&rarr;</span>{" "}
                          {entry.to_status}
                        </span>
                      )}
                      <div className="flex items-center gap-1.5 ml-auto flex-shrink-0">
                        {isDenial && (
                          <span className="text-[10px] font-medium bg-[var(--color-accent-pink-dim)] text-[var(--color-accent-pink)] px-1.5 py-0.5 rounded">
                            denied
                          </span>
                        )}
                        {isError && entry.error && (
                          <span className="text-[11px] text-[var(--color-accent-pink)] truncate max-w-48">
                            {entry.error}
                          </span>
                        )}
                        {isDenial && entry.denied_resource && (
                          <span className="text-[10px] font-mono text-[var(--color-text-muted)] truncate max-w-32">
                            {entry.denied_resource}
                          </span>
                        )}
                        {entry.tenant && entry.tenant !== "default" && (
                          <span className="text-[10px] text-[var(--color-text-muted)] font-mono">
                            {entry.tenant}
                          </span>
                        )}
                      </div>
                    </div>
                  );
                })}
              </div>
            </div>
          ) : (
            <div className="glass rounded p-6 text-center">
              <p className="text-sm text-[var(--color-text-secondary)]">
                No action history recorded for this agent.
              </p>
            </div>
          )}
        </>
      )}

      {/* Policies tab */}
      {activeTab === "policies" && (
        <div>
          <div className="flex items-center justify-between mb-3">
            <p className="text-xs text-[var(--color-text-muted)]">
              Policies affecting this agent (direct or broad)
            </p>
            <button
              type="button"
              onClick={() => setShowPolicyCreator(!showPolicyCreator)}
              className="px-2.5 py-1.5 text-xs bg-[var(--color-accent-teal-dim)] text-[var(--color-accent-teal)] rounded-sm hover:bg-[var(--color-accent-teal-dim)] transition-colors"
            >
              {showPolicyCreator ? "Cancel" : "Add Policy"}
            </button>
          </div>

          {showPolicyCreator && (
            <VisualPolicyCreator
              specs={specs}
              tenants={tenants}
              onCreated={handlePolicyCreated}
              onCancel={() => setShowPolicyCreator(false)}
            />
          )}

          {agentPolicies.length > 0 ? (
            <div className="space-y-2">
              {agentPolicies.map((p) => (
                <div
                  key={`${p.tenant}:${p.policy_id}`}
                  className="glass rounded px-3.5 py-2.5 flex items-start gap-3"
                >
                  <div className="flex-1 min-w-0">
                    <div className="flex items-center gap-2">
                      <span className="font-mono text-[12px] text-[var(--color-text-primary)] truncate">
                        {p.policy_id}
                      </span>
                      <span className={`text-[10px] px-1.5 py-0.5 rounded-full ${
                        p.enabled
                          ? "bg-[var(--color-accent-teal-dim)] text-[var(--color-accent-teal)]"
                          : "bg-[var(--color-bg-elevated)] text-[var(--color-text-muted)]"
                      }`}>
                        {p.enabled ? "enabled" : "disabled"}
                      </span>
                      <span className="text-[10px] text-[var(--color-text-muted)] font-mono">
                        {p.tenant}
                      </span>
                      <span className="text-[10px] text-[var(--color-text-muted)]">
                        {p.source}
                      </span>
                    </div>
                    <pre className="mt-1 text-[10px] font-mono text-[var(--color-text-secondary)] whitespace-pre-wrap truncate max-h-16 overflow-hidden">
                      {p.cedar_text}
                    </pre>
                  </div>
                  <div className="flex items-center gap-1.5 flex-shrink-0">
                    <button
                      type="button"
                      onClick={() => handleTogglePolicy(p)}
                      className="text-[10px] px-2 py-1 bg-[var(--color-bg-elevated)] text-[var(--color-text-secondary)] rounded-sm hover:bg-[var(--color-border)] transition-colors"
                    >
                      {p.enabled ? "Disable" : "Enable"}
                    </button>
                    {p.source === "manual" && (
                      <button
                        type="button"
                        onClick={() => handleDeletePolicy(p)}
                        className="text-[10px] px-2 py-1 bg-[var(--color-accent-pink-dim)] text-[var(--color-accent-pink)] rounded-sm hover:bg-[var(--color-accent-pink-dim)] transition-colors"
                      >
                        Delete
                      </button>
                    )}
                  </div>
                </div>
              ))}
            </div>
          ) : (
            <div className="glass rounded p-6 text-center">
              <p className="text-sm text-[var(--color-text-secondary)]">
                No policies found for this agent.
              </p>
            </div>
          )}
        </div>
      )}
    </div>
  );
}
