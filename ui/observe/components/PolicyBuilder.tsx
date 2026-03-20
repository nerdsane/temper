"use client";

import { useState, useMemo } from "react";
import type {
  PendingDecision,
  PolicyScopeMatrix,
  PrincipalScope,
  ActionScopeOption,
  ResourceScopeOption,
  DurationScope,
} from "@/lib/types";
import { generatePolicyPreview } from "@/lib/utils";
import RadioGroup from "./RadioGroup";
import {
  PRINCIPAL_OPTIONS,
  ACTION_OPTIONS,
  RESOURCE_OPTIONS,
  DURATION_OPTIONS,
} from "@/lib/policy-options";

/** Context derived from a decision or provided explicitly for standalone mode. */
export interface PolicyBuilderContext {
  agentId?: string;
  agentType?: string;
  action?: string;
  resourceType?: string;
  resourceId?: string;
  sessionId?: string;
}

interface DecisionModeProps {
  decision: PendingDecision;
  context?: never;
  onApprove: (matrix: PolicyScopeMatrix) => void;
  onDeny: () => void;
  onCancel?: never;
  disabled?: boolean;
}

interface StandaloneModeProps {
  decision?: never;
  context: PolicyBuilderContext;
  onApprove: (matrix: PolicyScopeMatrix) => void;
  onDeny?: never;
  onCancel: () => void;
  disabled?: boolean;
}

type PolicyBuilderProps = DecisionModeProps | StandaloneModeProps;

export default function PolicyBuilder(props: PolicyBuilderProps) {
  const { onApprove, disabled } = props;

  // Normalize context from either decision or explicit context
  const ctx: PolicyBuilderContext = useMemo(() => {
    if (props.decision) {
      return {
        agentId: props.decision.agent_id,
        agentType: props.decision.agent_type,
        action: props.decision.action,
        resourceType: props.decision.resource_type,
        resourceId: props.decision.resource_id,
        sessionId: props.decision.session_id,
      };
    }
    return props.context;
  }, [props.decision, props.context]);

  // Default to broader scopes in standalone mode when fields are missing
  const defaultPrincipal: PrincipalScope = ctx.agentId ? "this_agent" : "any_agent";
  const defaultAction: ActionScopeOption = ctx.action ? "this_action" : "all_actions";
  const defaultResource: ResourceScopeOption = ctx.resourceType ? "any_of_type" : "any_resource";

  const [principal, setPrincipal] = useState<PrincipalScope>(defaultPrincipal);
  const [action, setAction] = useState<ActionScopeOption>(defaultAction);
  const [resource, setResource] = useState<ResourceScopeOption>(defaultResource);
  const [duration, setDuration] = useState<DurationScope>("always");
  const [showPreview, setShowPreview] = useState(false);

  const principalOptions = useMemo(() => {
    return PRINCIPAL_OPTIONS.filter((opt) => {
      if (opt.value === "agents_of_type" && !ctx.agentType) return false;
      return true;
    });
  }, [ctx.agentType]);

  const durationOptions = useMemo(() => {
    return DURATION_OPTIONS.filter((opt) => {
      if (opt.value === "session" && !ctx.sessionId) return false;
      return true;
    });
  }, [ctx.sessionId]);

  const matrix: PolicyScopeMatrix = useMemo(() => ({
    principal,
    action,
    resource,
    duration,
    agent_type_value: principal === "agents_of_type" ? ctx.agentType : undefined,
    session_id: duration === "session" ? ctx.sessionId : undefined,
  }), [principal, action, resource, duration, ctx.agentType, ctx.sessionId]);

  const preview = useMemo(
    () =>
      generatePolicyPreview(
        ctx.agentId || "agent",
        ctx.action || "*",
        ctx.resourceType || "Resource",
        ctx.resourceId || "*",
        matrix,
      ),
    [ctx, matrix],
  );

  return (
    <div className="space-y-3 pt-2">
      <RadioGroup label="Who" options={principalOptions} value={principal} onChange={setPrincipal} />
      <RadioGroup label="What action" options={ACTION_OPTIONS} value={action} onChange={setAction} />
      <RadioGroup label="Which resource" options={RESOURCE_OPTIONS} value={resource} onChange={setResource} />
      <RadioGroup label="How long" options={durationOptions} value={duration} onChange={setDuration} />

      {/* Cedar preview */}
      <div>
        <button
          type="button"
          onClick={() => setShowPreview(!showPreview)}
          className="text-[10px] text-[var(--color-text-secondary)] hover:text-[var(--color-text-secondary)] uppercase tracking-wider"
        >
          {showPreview ? "Hide" : "Show"} Cedar preview
        </button>
        {showPreview && (
          <pre className="mt-1.5 p-2 bg-black/30 rounded text-[11px] text-[var(--color-accent-teal)] font-mono overflow-x-auto opacity-80">
            {preview}
          </pre>
        )}
      </div>

      {/* Actions */}
      <div className="flex gap-2 pt-1">
        <button
          type="button"
          disabled={disabled}
          onClick={() => onApprove(matrix)}
          className="px-3 py-1.5 text-xs bg-[var(--color-accent-teal-dim)] text-[var(--color-accent-teal)] rounded hover:bg-[var(--color-accent-teal-dim)] disabled:opacity-50 transition-colors"
        >
          Approve
        </button>
        {props.decision ? (
          <button
            type="button"
            disabled={disabled}
            onClick={props.onDeny}
            className="px-3 py-1.5 text-xs bg-[var(--color-accent-pink-dim)] text-[var(--color-accent-pink)] rounded hover:bg-[var(--color-accent-pink-dim)] disabled:opacity-50 transition-colors"
          >
            Deny
          </button>
        ) : (
          <button
            type="button"
            disabled={disabled}
            onClick={props.onCancel}
            className="px-3 py-1.5 text-xs bg-[var(--color-bg-elevated)] text-[var(--color-text-secondary)] rounded hover:bg-[var(--color-border)] disabled:opacity-50 transition-colors"
          >
            Cancel
          </button>
        )}
      </div>
    </div>
  );
}
