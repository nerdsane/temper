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

interface PolicyBuilderProps {
  decision: PendingDecision;
  onApprove: (matrix: PolicyScopeMatrix) => void;
  onDeny: () => void;
  disabled?: boolean;
}

const PRINCIPAL_OPTIONS: { value: PrincipalScope; label: string; description: string }[] = [
  { value: "this_agent", label: "This agent", description: "Only the requesting agent" },
  { value: "agents_of_type", label: "Agents of type", description: "All agents of the same type" },
  { value: "agents_with_role", label: "Agents with role", description: "All agents sharing a role" },
  { value: "any_agent", label: "Any agent", description: "Any authenticated agent" },
];

const ACTION_OPTIONS: { value: ActionScopeOption; label: string; description: string }[] = [
  { value: "this_action", label: "This action only", description: "Only the denied action" },
  { value: "all_actions_on_type", label: "All actions on type", description: "Any action on this resource type" },
  { value: "all_actions", label: "All actions", description: "Any action on any resource" },
];

const RESOURCE_OPTIONS: { value: ResourceScopeOption; label: string; description: string }[] = [
  { value: "this_resource", label: "This resource", description: "Only the exact resource" },
  { value: "any_of_type", label: "Any of type", description: "Any resource of this type" },
  { value: "any_resource", label: "Any resource", description: "Any resource" },
];

const DURATION_OPTIONS: { value: DurationScope; label: string; description: string }[] = [
  { value: "always", label: "Always", description: "Permanent policy" },
  { value: "session", label: "This session", description: "Only the current session" },
];

function RadioGroup<T extends string>({
  label,
  options,
  value,
  onChange,
}: {
  label: string;
  options: { value: T; label: string; description: string }[];
  value: T;
  onChange: (v: T) => void;
}) {
  return (
    <div className="space-y-1.5">
      <div className="text-[10px] text-zinc-500 uppercase tracking-wider font-medium">{label}</div>
      <div className="flex flex-wrap gap-1.5">
        {options.map((opt) => (
          <button
            key={opt.value}
            type="button"
            onClick={() => onChange(opt.value)}
            title={opt.description}
            className={`px-2.5 py-1 text-[11px] rounded-sm transition-colors ${
              value === opt.value
                ? "bg-teal-500/20 text-teal-400 ring-1 ring-teal-500/30"
                : "bg-white/[0.03] text-zinc-500 hover:bg-white/[0.06] hover:text-zinc-400"
            }`}
          >
            {opt.label}
          </button>
        ))}
      </div>
    </div>
  );
}

export default function PolicyBuilder({ decision, onApprove, onDeny, disabled }: PolicyBuilderProps) {
  const [principal, setPrincipal] = useState<PrincipalScope>("this_agent");
  const [action, setAction] = useState<ActionScopeOption>("this_action");
  const [resource, setResource] = useState<ResourceScopeOption>("any_of_type");
  const [duration, setDuration] = useState<DurationScope>("always");
  const [showPreview, setShowPreview] = useState(false);

  // Filter principal options: only show "agents_of_type" when decision has agent_type
  const principalOptions = useMemo(() => {
    return PRINCIPAL_OPTIONS.filter((opt) => {
      if (opt.value === "agents_of_type" && !decision.agent_type) return false;
      return true;
    });
  }, [decision.agent_type]);

  // Filter duration options: only show "session" when session_id is available
  const durationOptions = useMemo(() => {
    return DURATION_OPTIONS.filter((opt) => {
      if (opt.value === "session" && !decision.session_id) return false;
      return true;
    });
  }, [decision.session_id]);

  const matrix: PolicyScopeMatrix = useMemo(() => ({
    principal,
    action,
    resource,
    duration,
    agent_type_value: principal === "agents_of_type" ? decision.agent_type : undefined,
    session_id: duration === "session" ? decision.session_id : undefined,
  }), [principal, action, resource, duration, decision.agent_type, decision.session_id]);

  const preview = useMemo(
    () =>
      generatePolicyPreview(
        decision.agent_id,
        decision.action,
        decision.resource_type,
        decision.resource_id,
        matrix,
      ),
    [decision, matrix],
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
          className="text-[10px] text-zinc-500 hover:text-zinc-400 uppercase tracking-wider"
        >
          {showPreview ? "Hide" : "Show"} Cedar preview
        </button>
        {showPreview && (
          <pre className="mt-1.5 p-2 bg-black/30 rounded text-[11px] text-teal-400/80 font-mono overflow-x-auto">
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
          className="px-3 py-1.5 text-xs bg-teal-500/20 text-teal-400 rounded hover:bg-teal-500/30 disabled:opacity-50 transition-colors"
        >
          Approve
        </button>
        <button
          type="button"
          disabled={disabled}
          onClick={onDeny}
          className="px-3 py-1.5 text-xs bg-pink-500/10 text-pink-400 rounded hover:bg-pink-500/20 disabled:opacity-50 transition-colors"
        >
          Deny
        </button>
      </div>
    </div>
  );
}
