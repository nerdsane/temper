import type { RadioOption } from "@/components/RadioGroup";
import type {
  PrincipalScope,
  ActionScopeOption,
  ResourceScopeOption,
  DurationScope,
} from "./types";

export const PRINCIPAL_OPTIONS: RadioOption<PrincipalScope>[] = [
  { value: "this_agent", label: "This agent", description: "Only the requesting agent" },
  { value: "agents_of_type", label: "Agents of type", description: "All agents of the same type" },
  { value: "agents_with_role", label: "Agents with role", description: "All agents sharing a role" },
  { value: "any_agent", label: "Any agent", description: "Any authenticated agent" },
];

export const ACTION_OPTIONS: RadioOption<ActionScopeOption>[] = [
  { value: "this_action", label: "This action only", description: "Only the denied action" },
  { value: "all_actions_on_type", label: "All actions on type", description: "Any action on this resource type" },
  { value: "all_actions", label: "All actions", description: "Any action on any resource" },
];

export const RESOURCE_OPTIONS: RadioOption<ResourceScopeOption>[] = [
  { value: "this_resource", label: "This resource", description: "Only the exact resource" },
  { value: "any_of_type", label: "Any of type", description: "Any resource of this type" },
  { value: "any_resource", label: "Any resource", description: "Any resource" },
];

export const DURATION_OPTIONS: RadioOption<DurationScope>[] = [
  { value: "always", label: "Always", description: "Permanent policy" },
  { value: "session", label: "This session", description: "Only the current session" },
];
