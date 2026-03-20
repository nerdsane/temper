"use client";

import { useState, useMemo, useEffect, useCallback } from "react";
import type {
  SpecSummary,
  PrincipalScope,
  DurationScope,
  AgentSummary,
} from "@/lib/types";
import { fetchAgents } from "@/lib/api";
import RadioGroup from "./RadioGroup";
import PermissionsMatrix, {
  emptySelection,
  countPolicies,
  type PermissionsSelection,
} from "./PermissionsMatrix";
import { PRINCIPAL_OPTIONS, DURATION_OPTIONS } from "@/lib/policy-options";

interface VisualPolicyCreatorProps {
  specs: SpecSummary[];
  tenants: string[];
  onCreated: (tenant: string, policyId: string, cedarText: string) => Promise<void>;
  onCancel: () => void;
}

/** Generate Cedar text for one permission entry */
function generateCedar(
  principal: PrincipalScope,
  agentId: string,
  agentTypeInput: string,
  entityType: string,
  action: string | null, // null = all actions
  duration: DurationScope,
  sessionId?: string,
): string {
  let principalClause: string;
  switch (principal) {
    case "this_agent":
      principalClause = `principal == Agent::"${agentId}"`;
      break;
    case "agents_of_type":
      principalClause = "principal is Agent";
      break;
    case "agents_with_role":
      principalClause = "principal is Agent";
      break;
    case "any_agent":
      principalClause = "principal is Agent";
      break;
  }

  const actionClause = action
    ? `action == Action::"${action}"`
    : "action";

  const resourceClause = `resource is ${entityType}`;

  const conditions: string[] = [];
  if (principal === "agents_of_type" && agentTypeInput) {
    conditions.push(`context.agentType == "${agentTypeInput}"`);
  }
  if (principal === "agents_with_role" && agentTypeInput) {
    conditions.push(`context.role == "${agentTypeInput}"`);
  }
  if (duration === "session" && sessionId) {
    conditions.push(`context.sessionId == "${sessionId}"`);
  }

  const whenClause = conditions.length > 0
    ? `\nwhen { ${conditions.join(" && ")} }`
    : "";

  return `permit(\n  ${principalClause},\n  ${actionClause},\n  ${resourceClause}\n)${whenClause};`;
}

/** Generate a policy ID from components */
function makePolicyId(
  principal: PrincipalScope,
  agentValue: string,
  entityType: string,
  action: string | null,
): string {
  const parts = ["custom"];
  switch (principal) {
    case "this_agent":
      parts.push(agentValue || "agent");
      break;
    case "agents_of_type":
      parts.push(agentValue || "type");
      break;
    case "agents_with_role":
      parts.push(agentValue || "role");
      break;
    case "any_agent":
      parts.push("any-agent");
      break;
  }
  parts.push(entityType);
  parts.push(action || "all-actions");
  return parts.join(":");
}

export default function VisualPolicyCreator({
  specs,
  tenants,
  onCreated,
  onCancel,
}: VisualPolicyCreatorProps) {
  const [principal, setPrincipal] = useState<PrincipalScope>("any_agent");
  const [duration, setDuration] = useState<DurationScope>("always");

  const [selectedTenant, setSelectedTenant] = useState(tenants[0] || "");
  const [selectedAgentId, setSelectedAgentId] = useState("");
  const [agentTypeInput, setAgentTypeInput] = useState("");
  const [permissions, setPermissions] = useState<PermissionsSelection>(emptySelection());
  const [showCedar, setShowCedar] = useState(false);
  const [creating, setCreating] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [result, setResult] = useState<{ succeeded: number; failed: number } | null>(null);

  // Fetch agents scoped to selected tenant
  const [agents, setAgents] = useState<AgentSummary[]>([]);
  useEffect(() => {
    if (!selectedTenant) return;
    fetchAgents({ tenant: selectedTenant })
      .then((res) => setAgents(res.agents))
      .catch(() => setAgents([]));
  }, [selectedTenant]);

  // Reset permissions when tenant changes
  useEffect(() => {
    setPermissions(emptySelection());
  }, [selectedTenant]);

  const agentValue = principal === "this_agent"
    ? selectedAgentId
    : principal === "agents_of_type" || principal === "agents_with_role"
      ? agentTypeInput
      : "";

  const policyCount = countPolicies(permissions);

  // Generate all Cedar policies from current selection
  const generatedPolicies = useMemo(() => {
    const policies: { id: string; cedar: string }[] = [];
    for (const [entityType, entry] of permissions.entities) {
      if (entry.allActions) {
        policies.push({
          id: makePolicyId(principal, agentValue, entityType, null),
          cedar: generateCedar(principal, selectedAgentId, agentTypeInput, entityType, null, duration),
        });
      } else {
        for (const action of entry.actions) {
          policies.push({
            id: makePolicyId(principal, agentValue, entityType, action),
            cedar: generateCedar(principal, selectedAgentId, agentTypeInput, entityType, action, duration),
          });
        }
      }
    }
    return policies;
  }, [permissions, principal, agentValue, selectedAgentId, agentTypeInput, duration]);

  const handleCreate = useCallback(async () => {
    if (!selectedTenant || policyCount === 0) return;
    setCreating(true);
    setError(null);
    setResult(null);

    const results = await Promise.allSettled(
      generatedPolicies.map((p) => onCreated(selectedTenant, p.id, p.cedar)),
    );

    const succeeded = results.filter((r) => r.status === "fulfilled").length;
    const failed = results.filter((r) => r.status === "rejected").length;

    if (failed > 0) {
      const firstError = results.find((r) => r.status === "rejected") as PromiseRejectedResult;
      setError(`${failed} of ${results.length} failed: ${firstError.reason?.message || "Unknown error"}`);
    }
    setResult({ succeeded, failed });
    setCreating(false);

    if (failed === 0) {
      setTimeout(() => {
        setPermissions(emptySelection());
        setResult(null);
      }, 1500);
    }
  }, [selectedTenant, policyCount, generatedPolicies, onCreated]);

  return (
    <div className="glass rounded p-4 mb-6 animate-fade-in">
      <h3 className="text-sm font-semibold text-[var(--color-text-primary)] mb-4">
        Create Policies
      </h3>

      <div className="space-y-4">
        {/* Tenant selector */}
        <div>
          <div className="text-[10px] text-[var(--color-text-secondary)] uppercase tracking-wider font-medium mb-1.5">
            Tenant
          </div>
          <select
            value={selectedTenant}
            onChange={(e) => setSelectedTenant(e.target.value)}
            className="bg-[var(--color-bg-surface)] text-[var(--color-text-primary)] text-xs rounded-sm px-2.5 py-1.5 focus:outline-none focus:ring-1 focus:ring-[var(--color-accent-teal)]"
          >
            {tenants.map((t) => (
              <option key={t} value={t}>{t}</option>
            ))}
          </select>
        </div>

        {/* WHO — principal scope */}
        <div>
          <RadioGroup
            label="Allow who"
            options={PRINCIPAL_OPTIONS}
            value={principal}
            onChange={setPrincipal}
          />
          {principal === "this_agent" && (
            <div className="mt-2">
              <select
                value={selectedAgentId}
                onChange={(e) => setSelectedAgentId(e.target.value)}
                className="bg-[var(--color-bg-surface)] text-[var(--color-text-primary)] text-xs rounded-sm px-2.5 py-1.5 font-mono focus:outline-none focus:ring-1 focus:ring-[var(--color-accent-teal)]"
              >
                <option value="">Select agent...</option>
                {agents.map((a) => (
                  <option key={a.agent_id} value={a.agent_id}>{a.agent_id}</option>
                ))}
              </select>
              {agents.length === 0 && (
                <span className="text-[10px] text-[var(--color-text-muted)] ml-2">
                  No agents found for this tenant
                </span>
              )}
            </div>
          )}
          {principal === "agents_of_type" && (
            <div className="mt-2">
              <input
                type="text"
                value={agentTypeInput}
                onChange={(e) => setAgentTypeInput(e.target.value)}
                placeholder="e.g., claude-code"
                className="bg-[var(--color-bg-surface)] text-[var(--color-text-primary)] text-xs rounded-sm px-2.5 py-1.5 font-mono focus:outline-none focus:ring-1 focus:ring-[var(--color-accent-teal)] placeholder:text-[var(--color-text-muted)] w-48"
              />
            </div>
          )}
          {principal === "agents_with_role" && (
            <div className="mt-2">
              <input
                type="text"
                value={agentTypeInput}
                onChange={(e) => setAgentTypeInput(e.target.value)}
                placeholder="e.g., operations_agent"
                className="bg-[var(--color-bg-surface)] text-[var(--color-text-primary)] text-xs rounded-sm px-2.5 py-1.5 font-mono focus:outline-none focus:ring-1 focus:ring-[var(--color-accent-teal)] placeholder:text-[var(--color-text-muted)] w-48"
              />
            </div>
          )}
        </div>

        {/* PERMISSIONS MATRIX */}
        <PermissionsMatrix
          specs={specs}
          tenant={selectedTenant}
          value={permissions}
          onChange={setPermissions}
        />

        {/* DURATION */}
        <RadioGroup
          label="For how long"
          options={DURATION_OPTIONS}
          value={duration}
          onChange={setDuration}
        />

        {/* Cedar preview */}
        <div>
          <button
            type="button"
            onClick={() => setShowCedar(!showCedar)}
            className="text-[10px] text-[var(--color-text-secondary)] hover:text-[var(--color-text-primary)] uppercase tracking-wider"
          >
            {showCedar ? "Hide" : "Show"} Cedar preview ({policyCount} {policyCount === 1 ? "policy" : "policies"})
          </button>
          {showCedar && generatedPolicies.length > 0 && (
            <div className="mt-1.5 space-y-2">
              {generatedPolicies.map((p) => (
                <div key={p.id}>
                  <div className="text-[10px] font-mono text-[var(--color-text-muted)] mb-0.5">
                    {p.id}
                  </div>
                  <pre className="p-2.5 bg-black/30 rounded text-[11px] text-[var(--color-accent-teal)] font-mono overflow-x-auto border border-[var(--color-border)]">
                    {p.cedar}
                  </pre>
                </div>
              ))}
            </div>
          )}
        </div>

        {/* Error */}
        {error && (
          <div className="text-xs text-[var(--color-accent-pink)]">{error}</div>
        )}

        {/* Result */}
        {result && (
          <div className={`text-xs font-mono ${result.failed > 0 ? "text-[var(--color-accent-pink)]" : "text-[var(--color-accent-teal)]"}`}>
            {result.succeeded} created{result.failed > 0 ? `, ${result.failed} failed` : ""}
          </div>
        )}

        {/* Actions */}
        <div className="flex gap-2 pt-1">
          <button
            type="button"
            disabled={creating || !selectedTenant || policyCount === 0}
            onClick={handleCreate}
            className="px-3 py-1.5 text-xs bg-[var(--color-accent-teal-dim)] text-[var(--color-accent-teal)] rounded hover:bg-[var(--color-accent-teal-dim)] disabled:opacity-50 transition-colors"
          >
            {creating ? "Creating..." : `Create ${policyCount} ${policyCount === 1 ? "policy" : "policies"}`}
          </button>
          <button
            type="button"
            onClick={onCancel}
            className="px-3 py-1.5 text-xs bg-[var(--color-bg-elevated)] text-[var(--color-text-secondary)] rounded hover:bg-[var(--color-border)] transition-colors"
          >
            Cancel
          </button>
        </div>
      </div>
    </div>
  );
}
