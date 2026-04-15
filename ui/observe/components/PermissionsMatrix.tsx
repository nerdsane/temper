"use client";

import { useCallback, useMemo } from "react";
import type { SpecSummary } from "@/lib/types";

/** Which entity types and actions are checked */
export interface PermissionsSelection {
  /** Map of entityType -> { allActions: whether header is checked, actions: set of checked actions } */
  entities: Map<string, { allActions: boolean; actions: Set<string> }>;
}

export function emptySelection(): PermissionsSelection {
  return { entities: new Map() };
}

/** Count how many policies will be generated from a selection */
export function countPolicies(selection: PermissionsSelection): number {
  let count = 0;
  for (const [, entry] of selection.entities) {
    if (entry.allActions) {
      count += 1; // one broad policy
    } else if (entry.actions.size > 0) {
      count += entry.actions.size; // one per action
    }
  }
  return count;
}

interface PermissionsMatrixProps {
  specs: SpecSummary[];
  tenant: string;
  value: PermissionsSelection;
  onChange: (value: PermissionsSelection) => void;
}

export default function PermissionsMatrix({
  specs,
  tenant,
  value,
  onChange,
}: PermissionsMatrixProps) {
  // Group specs by entity type for the selected tenant
  const entityGroups = useMemo(() => {
    const groups = new Map<string, string[]>();
    for (const s of specs) {
      if (s.tenant !== tenant) continue;
      const existing = groups.get(s.entity_type);
      if (existing) {
        // Merge actions
        for (const a of s.actions) {
          if (!existing.includes(a)) existing.push(a);
        }
      } else {
        groups.set(s.entity_type, [...s.actions]);
      }
    }
    // Sort actions within each group
    for (const [, actions] of groups) {
      actions.sort();
    }
    return groups;
  }, [specs, tenant]);

  const toggleEntityType = useCallback(
    (entityType: string) => {
      const next = new Map(value.entities);
      const current = next.get(entityType);
      if (current?.allActions) {
        // Uncheck everything for this type
        next.delete(entityType);
      } else {
        // Check "all actions" for this type
        next.set(entityType, { allActions: true, actions: new Set() });
      }
      onChange({ entities: next });
    },
    [value, onChange],
  );

  const toggleAction = useCallback(
    (entityType: string, action: string) => {
      const next = new Map(value.entities);
      const current = next.get(entityType) || { allActions: false, actions: new Set<string>() };
      const nextActions = new Set(current.actions);

      if (current.allActions) {
        // Was "all actions" — switching to individual mode, unchecking this action
        const allActions = entityGroups.get(entityType) || [];
        for (const a of allActions) {
          if (a !== action) nextActions.add(a);
        }
        next.set(entityType, { allActions: false, actions: nextActions });
      } else if (nextActions.has(action)) {
        nextActions.delete(action);
        if (nextActions.size === 0) {
          next.delete(entityType);
        } else {
          next.set(entityType, { allActions: false, actions: nextActions });
        }
      } else {
        nextActions.add(action);
        // If all actions are now checked, promote to allActions
        const allActions = entityGroups.get(entityType) || [];
        if (nextActions.size === allActions.length) {
          next.set(entityType, { allActions: true, actions: new Set() });
        } else {
          next.set(entityType, { allActions: false, actions: nextActions });
        }
      }
      onChange({ entities: next });
    },
    [value, onChange, entityGroups],
  );

  if (entityGroups.size === 0) {
    return (
      <div className="text-xs text-[var(--color-text-muted)] py-2">
        No entity types found for this tenant.
      </div>
    );
  }

  return (
    <div className="space-y-1">
      <div className="text-[10px] text-[var(--color-text-secondary)] uppercase tracking-wider font-medium mb-1.5">
        Permissions
      </div>
      <div className="bg-[var(--color-bg-surface)] rounded border border-[var(--color-border)] divide-y divide-[var(--color-border)]">
        {Array.from(entityGroups.entries()).map(([entityType, actions]) => {
          const entry = value.entities.get(entityType);
          const isAllActions = entry?.allActions ?? false;
          const checkedActions = entry?.actions ?? new Set<string>();
          const someChecked = checkedActions.size > 0 || isAllActions;
          const isIndeterminate = !isAllActions && checkedActions.size > 0;

          return (
            <div key={entityType} className="px-3 py-2">
              {/* Entity type header */}
              <label className="flex items-center gap-2 cursor-pointer">
                <input
                  type="checkbox"
                  checked={isAllActions}
                  ref={(el) => {
                    if (el) el.indeterminate = isIndeterminate;
                  }}
                  onChange={() => toggleEntityType(entityType)}
                  className="accent-[var(--color-accent-teal)] flex-shrink-0"
                />
                <span className="font-mono text-[12px] text-[var(--color-text-primary)] font-medium">
                  {entityType}
                </span>
                {isAllActions && (
                  <span className="text-[10px] text-[var(--color-accent-teal)] ml-1">
                    (all actions)
                  </span>
                )}
                {isIndeterminate && (
                  <span className="text-[10px] text-[var(--color-text-muted)] ml-1">
                    ({checkedActions.size} of {actions.length})
                  </span>
                )}
              </label>

              {/* Individual actions — show when type has some selection or is expanded */}
              {someChecked && (
                <div className="ml-5 mt-1.5 flex flex-wrap gap-x-4 gap-y-1">
                  {actions.map((action) => {
                    const checked = isAllActions || checkedActions.has(action);
                    return (
                      <label key={action} className="flex items-center gap-1.5 cursor-pointer">
                        <input
                          type="checkbox"
                          checked={checked}
                          onChange={() => toggleAction(entityType, action)}
                          className="accent-[var(--color-accent-teal)] flex-shrink-0"
                        />
                        <span className={`font-mono text-[11px] ${checked ? "text-[var(--color-text-primary)]" : "text-[var(--color-text-muted)]"}`}>
                          {action}
                        </span>
                      </label>
                    );
                  })}
                </div>
              )}
            </div>
          );
        })}
      </div>
    </div>
  );
}
