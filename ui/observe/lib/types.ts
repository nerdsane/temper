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
  tenant: string;
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

// --- WASM integration types ---
export interface WasmModule {
  tenant: string;
  module_name: string;
  sha256_hash: string;
  cached: boolean;
  total_invocations: number;
  success_count: number;
  success_rate: number;
  last_invoked_at: string | null;
}

export interface WasmModulesResponse {
  modules: WasmModule[];
  total: number;
}

export interface WasmInvocation {
  timestamp: string;
  tenant: string;
  entity_type: string;
  entity_id: string;
  module_name: string;
  trigger_action: string;
  callback_action: string | null;
  success: boolean;
  error: string | null;
  duration_ms: number;
}

export interface WasmInvocationsResponse {
  invocations: WasmInvocation[];
  total: number;
}

// --- Pending Decision types ---
export type DecisionStatus = "pending" | "approved" | "denied" | "expired";

export type PrincipalScope = "this_agent" | "agents_with_role" | "agents_of_type" | "any_agent";
export type ActionScopeOption = "this_action" | "all_actions_on_type" | "all_actions";
export type ResourceScopeOption = "this_resource" | "any_of_type" | "any_resource";
export type DurationScope = "session" | "always";

export interface PolicyScopeMatrix {
  principal: PrincipalScope;
  action: ActionScopeOption;
  resource: ResourceScopeOption;
  duration: DurationScope;
  agent_type_value?: string;
  role_value?: string;
  session_id?: string;
}

export interface PendingDecision {
  id: string;
  tenant: string;
  agent_id: string;
  action: string;
  resource_type: string;
  resource_id: string;
  resource_attrs: Record<string, unknown>;
  denial_reason: string;
  module_name?: string;
  agent_type?: string;
  session_id?: string;
  created_at: string;
  status: DecisionStatus;
  decided_by?: string;
  decided_at?: string;
  generated_policy?: string;
  approved_scope?: PolicyScopeMatrix;
}

export interface DecisionsResponse {
  decisions: PendingDecision[];
  total: number;
  pending_count: number;
  approved_count: number;
  denied_count: number;
}

// --- Policy types ---
export type PolicySource = "manual" | "decision" | "os-app" | "migrated-legacy";

export interface PolicyEntry {
  policy_id: string;
  tenant: string;
  cedar_text: string;
  enabled: boolean;
  policy_hash: string;
  created_at: string;
  created_by: string;
  source: PolicySource;
}

export interface PoliciesResponse {
  tenant: string;
  policies: PolicyEntry[];
  total: number;
  enabled_count: number;
  disabled_count: number;
}

export interface AllPoliciesResponse {
  policies: PolicyEntry[];
  total: number;
  by_tenant: Record<string, number>;
}

// --- Agent types ---
export interface AgentSummary {
  agent_id: string;
  total_actions: number;
  success_count: number;
  error_count: number;
  denial_count: number;
  success_rate: number;
  last_active_at: string | null;
  entity_types: string[];
  tenants: string[];
}

export interface AgentHistoryEntry {
  timestamp: string;
  tenant: string;
  entity_type: string;
  entity_id: string;
  action: string;
  success: boolean;
  from_status: string | null;
  to_status: string | null;
  error: string | null;
  authz_denied: boolean;
  denied_resource: string | null;
}

export interface AgentsResponse {
  agents: AgentSummary[];
  total: number;
}

export interface AgentHistoryResponse {
  agent_id: string;
  history: AgentHistoryEntry[];
  total: number;
}

// --- Unmet Intent types ---
export interface UnmetIntent {
  entity_type: string;
  action: string;
  error_pattern: string;
  failure_count: number;
  first_seen: string;
  last_seen: string;
  status: "open" | "resolved";
  resolved_by?: string;
  recommendation: string;
}

export interface UnmetIntentsResponse {
  intents: UnmetIntent[];
  open_count: number;
  resolved_count: number;
}

// --- Feature Request types ---
export type PlatformGapCategory = 'MissingMethod' | 'GovernanceBlocked' | 'UnsupportedIntegration' | 'MissingCapability';
export type FeatureRequestDisposition = 'Open' | 'Acknowledged' | 'Planned' | 'WontFix' | 'Resolved';

export interface FeatureRequest {
  id: string;
  category: PlatformGapCategory;
  description: string;
  frequency: number;
  trajectory_refs: string[];
  disposition: FeatureRequestDisposition;
  developer_notes: string | null;
  created_at: string;
}

// --- Skill types ---
export interface Skill {
  name: string;
  description: string;
  entity_types: string[];
  version: string;
}

export interface SkillsResponse {
  apps: Skill[];
}

// Backward-compatible aliases.
export type OsApp = Skill;
export type OsAppsResponse = SkillsResponse;

// --- Extended evolution record detail ---
export interface EvolutionRecordDetail extends EvolutionRecord {
  derived_from?: string;
  evidence_query?: string;
  threshold_field?: string;
  threshold_value?: number;
  observed_value?: number;
  root_cause?: string;
  options?: { description: string; spec_diff?: string; risk: string; complexity: string }[];
  decision?: "Approved" | "Rejected" | "Deferred";
  decided_by?: string;
  rationale?: string;
  signal?: InsightSignal;
}

// --- Extended sentinel check response ---
export interface ExtendedSentinelCheckResponse extends SentinelCheckResponse {
  insights_count: number;
  insights: {
    record_id: string;
    category: string;
    intent: string;
    priority_score: number;
    recommendation: string;
  }[];
}
