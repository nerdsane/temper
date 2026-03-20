"use client";

import { useEffect, useState, useCallback, useMemo } from "react";
import {
  fetchPoliciesList,
  fetchAllPolicies,
  togglePolicy,
  deletePolicy,
  updatePolicyText,
  createPolicy,
  fetchSpecs,
} from "@/lib/api";
import { useSSERefresh } from "@/lib/hooks";
import type {
  PolicyEntry,
  PoliciesResponse,
  AllPoliciesResponse,
  PolicySource,
} from "@/lib/types";
import type { SpecSummary } from "@/lib/types";
import ErrorDisplay from "@/components/ErrorDisplay";
import StatCard from "@/components/StatCard";
import PolicyCard from "@/components/PolicyCard";
import VisualPolicyCreator from "@/components/VisualPolicyCreator";

const ALL_TENANTS = "__all__";
const SOURCE_FILTERS: { value: PolicySource | "all"; label: string }[] = [
  { value: "all", label: "All sources" },
  { value: "os-app", label: "Base (OS App)" },
  { value: "decision", label: "Approved" },
  { value: "manual", label: "Manual" },
  { value: "migrated-legacy", label: "Legacy" },
];

export default function PoliciesPage() {
  const [initialLoading, setInitialLoading] = useState(true);
  const [initialError, setInitialError] = useState<string | null>(null);
  const [tenant, setTenant] = useState<string>(ALL_TENANTS);
  const [tenants, setTenants] = useState<string[]>([]);
  const [actingIds, setActingIds] = useState<Set<string>>(new Set());
  const [actionError, setActionError] = useState<string | null>(null);
  const [searchQuery, setSearchQuery] = useState("");
  const [sourceFilter, setSourceFilter] = useState<PolicySource | "all">("all");
  const [statusFilter, setStatusFilter] = useState<"all" | "enabled" | "disabled">("all");
  const [showCreate, setShowCreate] = useState(false);
  const [specs, setSpecs] = useState<SpecSummary[]>([]);

  const loadInitial = useCallback(async () => {
    setInitialLoading(true);
    setInitialError(null);
    try {
      const loadedSpecs = await fetchSpecs();
      setSpecs(loadedSpecs);
      const tenantSet = new Set<string>();
      for (const s of loadedSpecs) {
        if (s.tenant && s.tenant !== "temper-system") tenantSet.add(s.tenant);
      }
      setTenants(Array.from(tenantSet).sort());
    } catch (err) {
      setInitialError(
        err instanceof Error ? err.message : "Failed to load policies",
      );
    } finally {
      setInitialLoading(false);
    }
  }, []);

  useEffect(() => {
    loadInitial();
  }, [loadInitial]);

  // Fetch policies for current view
  const policiesPoll = useSSERefresh<PoliciesResponse | AllPoliciesResponse>({
    fetcher: () =>
      tenant === ALL_TENANTS
        ? fetchAllPolicies()
        : fetchPoliciesList(tenant),
    sseKinds: ["Policies"],
    enabled: !initialLoading && !initialError,
  });

  const data = policiesPoll.data;

  // Normalize to a flat array of PolicyEntry
  const allPolicies: PolicyEntry[] = useMemo(() => {
    if (!data) return [];
    return (data as AllPoliciesResponse).policies || [];
  }, [data]);

  // Apply filters
  const filteredPolicies = useMemo(() => {
    let filtered = allPolicies;
    if (sourceFilter !== "all") {
      filtered = filtered.filter((p) => p.source === sourceFilter);
    }
    if (statusFilter === "enabled") {
      filtered = filtered.filter((p) => p.enabled);
    } else if (statusFilter === "disabled") {
      filtered = filtered.filter((p) => !p.enabled);
    }
    if (searchQuery.trim()) {
      const q = searchQuery.toLowerCase();
      filtered = filtered.filter(
        (p) =>
          p.cedar_text.toLowerCase().includes(q) ||
          p.policy_id.toLowerCase().includes(q) ||
          p.created_by.toLowerCase().includes(q) ||
          p.tenant.toLowerCase().includes(q),
      );
    }
    return filtered;
  }, [allPolicies, sourceFilter, statusFilter, searchQuery]);

  // Group by tenant when viewing all tenants
  const groupedByTenant = useMemo(() => {
    if (tenant !== ALL_TENANTS) return null;
    const groups = new Map<string, PolicyEntry[]>();
    for (const p of filteredPolicies) {
      const existing = groups.get(p.tenant) || [];
      existing.push(p);
      groups.set(p.tenant, existing);
    }
    return groups;
  }, [filteredPolicies, tenant]);

  const stats = useMemo(() => {
    const total = allPolicies.length;
    const enabled = allPolicies.filter((p) => p.enabled).length;
    const disabled = total - enabled;
    const bySource: Record<string, number> = {};
    for (const p of allPolicies) {
      bySource[p.source] = (bySource[p.source] || 0) + 1;
    }
    return { total, enabled, disabled, bySource };
  }, [allPolicies]);

  const handleToggle = useCallback(
    async (policyId: string, enabled: boolean) => {
      const policy = allPolicies.find((p) => p.policy_id === policyId);
      if (!policy) return;
      setActingIds((prev) => new Set(prev).add(policyId));
      setActionError(null);
      try {
        await togglePolicy(policy.tenant, policyId, enabled);
        await policiesPoll.refresh();
      } catch (err) {
        setActionError(
          err instanceof Error ? err.message : "Failed to toggle policy",
        );
      } finally {
        setActingIds((prev) => {
          const next = new Set(prev);
          next.delete(policyId);
          return next;
        });
      }
    },
    [allPolicies, policiesPoll],
  );

  const handleDelete = useCallback(
    async (policyId: string) => {
      const policy = allPolicies.find((p) => p.policy_id === policyId);
      if (!policy) return;
      setActingIds((prev) => new Set(prev).add(policyId));
      setActionError(null);
      try {
        await deletePolicy(policy.tenant, policyId);
        await policiesPoll.refresh();
      } catch (err) {
        setActionError(
          err instanceof Error ? err.message : "Failed to delete policy",
        );
      } finally {
        setActingIds((prev) => {
          const next = new Set(prev);
          next.delete(policyId);
          return next;
        });
      }
    },
    [allPolicies, policiesPoll],
  );

  const handleUpdate = useCallback(
    async (policyId: string, cedarText: string) => {
      const policy = allPolicies.find((p) => p.policy_id === policyId);
      if (!policy) return;
      setActingIds((prev) => new Set(prev).add(policyId));
      setActionError(null);
      try {
        await updatePolicyText(policy.tenant, policyId, cedarText);
        await policiesPoll.refresh();
      } catch (err) {
        setActionError(
          err instanceof Error ? err.message : "Failed to update policy",
        );
      } finally {
        setActingIds((prev) => {
          const next = new Set(prev);
          next.delete(policyId);
          return next;
        });
      }
    },
    [allPolicies, policiesPoll],
  );

  const handleVisualCreate = useCallback(
    async (targetTenant: string, policyId: string, cedarText: string) => {
      setActionError(null);
      await createPolicy(targetTenant, policyId, cedarText);
      setShowCreate(false);
      await policiesPoll.refresh();
    },
    [policiesPoll],
  );

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
        title="Cannot load policies"
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
            Policies
          </h1>
          <p className="text-sm text-[var(--color-text-muted)] mt-0.5">
            Cedar authorization policies governing agent actions
          </p>
        </div>
        <div className="flex items-center gap-3">
          <select
            value={tenant}
            onChange={(e) => setTenant(e.target.value)}
            className="bg-[var(--color-bg-surface)] text-[var(--color-text-secondary)] text-xs rounded-sm px-2 py-1.5 focus:outline-none"
          >
            <option value={ALL_TENANTS}>All tenants</option>
            {tenants.map((t) => (
              <option key={t} value={t}>
                {t}
              </option>
            ))}
          </select>
          <select
            value={sourceFilter}
            onChange={(e) =>
              setSourceFilter(e.target.value as PolicySource | "all")
            }
            className="bg-[var(--color-bg-surface)] text-[var(--color-text-secondary)] text-xs rounded-sm px-2 py-1.5 focus:outline-none"
          >
            {SOURCE_FILTERS.map((f) => (
              <option key={f.value} value={f.value}>
                {f.label}
              </option>
            ))}
          </select>
          <select
            value={statusFilter}
            onChange={(e) =>
              setStatusFilter(e.target.value as "all" | "enabled" | "disabled")
            }
            className="bg-[var(--color-bg-surface)] text-[var(--color-text-secondary)] text-xs rounded-sm px-2 py-1.5 focus:outline-none"
          >
            <option value="all">All statuses</option>
            <option value="enabled">Enabled</option>
            <option value="disabled">Disabled</option>
          </select>
          <input
            type="text"
            placeholder="Search policies..."
            value={searchQuery}
            onChange={(e) => setSearchQuery(e.target.value)}
            className="bg-[var(--color-bg-surface)] text-[var(--color-text-secondary)] text-xs rounded-sm px-2.5 py-1.5 w-48 focus:outline-none focus:ring-1 focus:ring-[var(--color-accent-teal)] placeholder:text-[var(--color-text-muted)]"
          />
          <button
            onClick={() => setShowCreate(!showCreate)}
            className="px-2.5 py-1.5 text-xs bg-[var(--color-accent-teal-dim)] text-[var(--color-accent-teal)] rounded-sm hover:bg-[var(--color-accent-teal-dim)] transition-colors"
          >
            {showCreate ? "Cancel" : "New Policy"}
          </button>
        </div>
      </div>

      {/* Stats row */}
      <div className="grid grid-cols-4 gap-3 mb-6">
        <StatCard label="Total" value={stats.total} />
        <StatCard
          label="Enabled"
          value={stats.enabled}
          color="text-[var(--color-accent-teal)]"
        />
        <StatCard
          label="Disabled"
          value={stats.disabled}
          color={
            stats.disabled > 0 ? "text-[var(--color-accent-pink)]" : undefined
          }
        />
        <StatCard
          label="Base"
          value={stats.bySource["os-app"] || 0}
        />
      </div>

      {/* Action error banner */}
      {actionError && (
        <div
          role="alert"
          className="mb-4 flex items-center justify-between gap-2 rounded bg-[var(--color-accent-pink-dim)] border border-[var(--color-accent-pink)]/20 px-4 py-2.5"
        >
          <p className="text-sm text-[var(--color-accent-pink)]">
            {actionError}
          </p>
          <button
            onClick={() => setActionError(null)}
            className="text-[var(--color-accent-pink)] hover:text-[var(--color-accent-pink)] text-xs flex-shrink-0"
            aria-label="Dismiss error"
          >
            Dismiss
          </button>
        </div>
      )}

      {/* Visual policy creator */}
      {showCreate && (
        <VisualPolicyCreator
          specs={specs}
          tenants={tenants}
          onCreated={handleVisualCreate}
          onCancel={() => setShowCreate(false)}
        />
      )}

      {/* Policy list */}
      {filteredPolicies.length === 0 && (
        <div className="glass rounded-[2px] p-6 text-center">
          <p className="text-sm text-[var(--color-text-secondary)]">
            {allPolicies.length === 0
              ? "No policies configured. Install an OS app or create a policy to get started."
              : "No policies match your filters."}
          </p>
        </div>
      )}

      {/* Grouped by tenant */}
      {groupedByTenant
        ? Array.from(groupedByTenant.entries()).map(
            ([tenantName, policies]) => (
              <div key={tenantName} className="mb-6">
                <div className="flex items-center gap-2 mb-3">
                  <span className="text-[11px] font-mono font-medium text-[var(--color-text-secondary)] uppercase tracking-wider">
                    {tenantName}
                  </span>
                  <div className="flex-1 h-px bg-[var(--color-bg-elevated)]" />
                  <span className="text-[10px] font-mono text-[var(--color-text-muted)]">
                    {policies.length}
                  </span>
                </div>
                <div className="grid gap-3">
                  {policies.map((p) => (
                    <PolicyCard
                      key={`${p.tenant}:${p.policy_id}`}
                      policy={p}
                      onToggle={handleToggle}
                      onDelete={handleDelete}
                      onUpdate={handleUpdate}
                      acting={actingIds.has(p.policy_id)}
                    />
                  ))}
                </div>
              </div>
            ),
          )
        : filteredPolicies.length > 0 && (
            <div className="grid gap-3">
              {filteredPolicies.map((p) => (
                <PolicyCard
                  key={`${p.tenant}:${p.policy_id}`}
                  policy={p}
                  onToggle={handleToggle}
                  onDelete={handleDelete}
                  onUpdate={handleUpdate}
                  acting={actingIds.has(p.policy_id)}
                />
              ))}
            </div>
          )}
    </div>
  );
}
