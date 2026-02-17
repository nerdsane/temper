export interface SpecSummary {
  tenant?: string;
  entity_type: string;
  states: string[];
  actions: string[];
  initial_state: string;
  verification_status: string;
  levels_passed?: number;
  levels_total?: number;
}

export interface SpecAction {
  name: string;
  kind: string;
  from: string[];
  to: string | null;
  guards: string[];
  effects: string[];
}

export interface SpecInvariant {
  name: string;
  when: string[];
  assertion: string;
}

export interface StateVariable {
  name: string;
  var_type: string;
  initial: string;
}

export interface SpecDetail {
  entity_type: string;
  states: string[];
  initial_state: string;
  actions: SpecAction[];
  invariants: SpecInvariant[];
  state_variables: StateVariable[];
}

export interface EntitySummary {
  entity_type: string;
  entity_id: string;
  actor_status: string;
  current_state?: string;
}

export interface VerificationLevel {
  level: string;
  passed: boolean;
  summary: string;
  details?: string;
  duration_ms?: number;
}

export interface VerificationResult {
  all_passed: boolean;
  levels: VerificationLevel[];
}

export interface EntityEvent {
  timestamp: string;
  action: string;
  from_state: string;
  to_state: string;
  actor: string;
}

export interface EntityHistory {
  entity_type: string;
  entity_id: string;
  current_state: string;
  events: EntityEvent[];
}

export interface EntityVerificationStatus {
  tenant: string;
  entity_type: string;
  status: string;
  levels?: { level: string; passed: boolean; summary: string }[];
  verified_at?: string;
}

export interface AllVerificationStatus {
  pending: number;
  running: number;
  passed: number;
  failed: number;
  partial: number;
  entities: EntityVerificationStatus[];
}

export interface DesignTimeEvent {
  kind: string;
  entity_type: string;
  tenant: string;
  summary: string;
  level?: string;
  passed?: boolean;
  timestamp?: string;
  step_number?: number;
  total_steps?: number;
}

export interface WorkflowStep {
  step: string;
  status: "pending" | "running" | "completed" | "failed";
  passed?: boolean;
  timestamp?: string;
  summary?: string;
}

export interface EntityWorkflow {
  entity_type: string;
  steps: WorkflowStep[];
}

export interface AppWorkflow {
  tenant: string;
  status: "loading" | "verifying" | "completed" | "failed";
  entities: EntityWorkflow[];
  runtime_events_count: number;
}

export interface WorkflowsResponse {
  workflows: AppWorkflow[];
}
