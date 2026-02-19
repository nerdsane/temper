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

export interface SmtState {
  status: string;
  counters: Record<string, number>;
  booleans: Record<string, boolean>;
}

export interface SmtDetails {
  guard_satisfiability: [string, boolean][];
  inductive_invariants: [string, boolean][];
  unreachable_states: string[];
  all_passed: boolean;
}

export interface Counterexample {
  property: string;
  trace: SmtState[];
}

export interface ModelCheckDetails {
  states_explored: number;
  all_properties_hold: boolean;
  counterexamples: Counterexample[];
  is_complete: boolean;
}

export interface LivenessViolation {
  actor_id: string;
  property: string;
  description: string;
  final_state: SmtState;
}

export interface InvariantViolation {
  actor_id: string;
  action: string;
  invariant: string;
  state_before: SmtState;
  state_after: SmtState;
  tick: number;
}

export interface SimulationDetails {
  all_invariants_held: boolean;
  ticks: number;
  total_transitions: number;
  total_messages: number;
  total_dropped: number;
  violations: InvariantViolation[];
  liveness_violations: LivenessViolation[];
  seed: number;
}

export interface PropTestFailure {
  invariant: string;
  action_sequence: string[];
  final_state: string;
}

export interface PropTestDetails {
  total_cases: number;
  passed: boolean;
  failure: PropTestFailure | null;
}

export interface VerificationDetail {
  kind: string;
  property: string;
  description: string;
  actor_id?: string;
}

export interface VerificationLevel {
  level: string;
  passed: boolean;
  summary: string;
  details?: VerificationDetail[] | string;
  duration_ms?: number;
  smt?: SmtDetails | null;
  verification?: ModelCheckDetails | null;
  simulation?: SimulationDetails | null;
  prop_test?: PropTestDetails | null;
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
  fields?: Record<string, unknown>;
  counters?: Record<string, number>;
  booleans?: Record<string, boolean>;
  lists?: Record<string, string[]>;
  events: EntityEvent[];
}

export interface EntityVerificationStatus {
  tenant: string;
  entity_type: string;
  status: string;
  levels?: VerificationLevel[];
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

export interface EntityStateChange {
  entity_type: string;
  entity_id: string;
  action: string;
  status: string;
  tenant: string;
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

// --- Trajectory types ---
export interface TrajectoryActionBreakdown {
  total: number;
  success: number;
  error: number;
}

export interface FailedIntent {
  timestamp: string;
  tenant: string;
  entity_type: string;
  entity_id: string;
  action: string;
  from_status: string | null;
  error: string;
}

export interface TrajectoryResponse {
  total: number;
  success_count: number;
  error_count: number;
  success_rate: number;
  by_action: Record<string, TrajectoryActionBreakdown>;
  failed_intents: FailedIntent[];
}

// --- Evolution types ---
export interface EvolutionRecord {
  id: string;
  record_type: string;
  status: string;
  created_by?: string;
  timestamp?: string;
  source?: string;
  classification?: string;
  category?: string;
  priority_score?: number;
  recommendation?: string;
  count?: number;
  note?: string;
}

export interface EvolutionRecordsResponse {
  records: EvolutionRecord[];
  total_observations: number;
  total_problems: number;
  total_analyses: number;
  total_decisions: number;
  total_insights: number;
}

export interface InsightSignal {
  intent: string;
  volume: number;
  success_rate: number;
  trend: string;
  growth_rate?: number;
}

export interface EvolutionInsight {
  id: string;
  category: string;
  priority_score: number;
  recommendation: string;
  signal: InsightSignal;
  status: string;
  timestamp: string;
}

export interface EvolutionInsightsResponse {
  insights: EvolutionInsight[];
  total: number;
}

// --- Sentinel types ---
export interface SentinelAlert {
  rule: string;
  record_id: string;
  source: string;
  classification: string;
  threshold: number;
  observed: number;
}

export interface SentinelCheckResponse {
  alerts_count: number;
  alerts: SentinelAlert[];
}
