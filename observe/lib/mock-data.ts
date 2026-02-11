// Mock data matching the Temper observe API shapes.
// Used as fallback when the backend is unavailable.

export interface SpecSummary {
  tenant?: string;
  entity_type: string;
  states: string[];
  actions: string[];
  initial_state: string;
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

// --- Mock Specs ---

export const MOCK_SPECS: SpecSummary[] = [
  {
    entity_type: "Ticket",
    states: ["Open", "InProgress", "Review", "Closed", "Cancelled"],
    actions: ["assign", "start_work", "submit_review", "approve", "reject", "cancel", "reopen"],
    initial_state: "Open",
  },
  {
    entity_type: "Invoice",
    states: ["Draft", "Sent", "Paid", "Overdue", "Voided"],
    actions: ["finalize", "send", "record_payment", "mark_overdue", "void", "resend"],
    initial_state: "Draft",
  },
  {
    entity_type: "Order",
    states: ["Pending", "Confirmed", "Shipped", "Delivered", "Returned"],
    actions: ["confirm", "ship", "deliver", "return_order", "cancel"],
    initial_state: "Pending",
  },
];

export const MOCK_SPEC_DETAILS: Record<string, SpecDetail> = {
  Ticket: {
    entity_type: "Ticket",
    states: ["Open", "InProgress", "Review", "Closed", "Cancelled"],
    initial_state: "Open",
    actions: [
      { name: "assign", kind: "input", from: ["Open"], to: "Open", guards: ["assignee != null"], effects: ["set assignee"] },
      { name: "start_work", kind: "input", from: ["Open"], to: "InProgress", guards: ["assignee != null"], effects: [] },
      { name: "submit_review", kind: "input", from: ["InProgress"], to: "Review", guards: [], effects: ["set review_requested_at"] },
      { name: "approve", kind: "input", from: ["Review"], to: "Closed", guards: ["reviewer != assignee"], effects: ["set closed_at"] },
      { name: "reject", kind: "input", from: ["Review"], to: "InProgress", guards: [], effects: ["increment rejection_count"] },
      { name: "cancel", kind: "input", from: ["Open", "InProgress"], to: "Cancelled", guards: [], effects: ["set cancelled_at"] },
      { name: "reopen", kind: "input", from: ["Closed"], to: "Open", guards: ["days_since_closed < 30"], effects: ["clear closed_at"] },
    ],
    invariants: [
      { name: "assigned_before_progress", when: ["InProgress"], assertion: "assignee != null" },
      { name: "review_needs_reviewer", when: ["Review"], assertion: "reviewer != null" },
      { name: "no_further_transitions", when: ["Cancelled"], assertion: "no outgoing transitions except reopen" },
    ],
    state_variables: [
      { name: "assignee", var_type: "Option<UserId>", initial: "null" },
      { name: "reviewer", var_type: "Option<UserId>", initial: "null" },
      { name: "rejection_count", var_type: "u32", initial: "0" },
      { name: "closed_at", var_type: "Option<Timestamp>", initial: "null" },
      { name: "cancelled_at", var_type: "Option<Timestamp>", initial: "null" },
      { name: "review_requested_at", var_type: "Option<Timestamp>", initial: "null" },
    ],
  },
  Invoice: {
    entity_type: "Invoice",
    states: ["Draft", "Sent", "Paid", "Overdue", "Voided"],
    initial_state: "Draft",
    actions: [
      { name: "finalize", kind: "input", from: ["Draft"], to: "Draft", guards: ["line_items.len() > 0"], effects: ["compute total"] },
      { name: "send", kind: "input", from: ["Draft"], to: "Sent", guards: ["total > 0"], effects: ["set sent_at"] },
      { name: "record_payment", kind: "input", from: ["Sent", "Overdue"], to: "Paid", guards: ["amount == total"], effects: ["set paid_at"] },
      { name: "mark_overdue", kind: "internal", from: ["Sent"], to: "Overdue", guards: ["days_since_sent > 30"], effects: ["set overdue_at"] },
      { name: "void", kind: "input", from: ["Draft", "Sent"], to: "Voided", guards: [], effects: ["set voided_at"] },
      { name: "resend", kind: "input", from: ["Sent"], to: "Sent", guards: [], effects: ["update sent_at"] },
    ],
    invariants: [
      { name: "paid_has_amount", when: ["Paid"], assertion: "paid_amount == total" },
      { name: "no_further_transitions", when: ["Voided"], assertion: "no outgoing transitions" },
      { name: "sent_has_total", when: ["Sent"], assertion: "total > 0" },
    ],
    state_variables: [
      { name: "total", var_type: "Decimal", initial: "0" },
      { name: "paid_amount", var_type: "Decimal", initial: "0" },
      { name: "sent_at", var_type: "Option<Timestamp>", initial: "null" },
      { name: "paid_at", var_type: "Option<Timestamp>", initial: "null" },
      { name: "overdue_at", var_type: "Option<Timestamp>", initial: "null" },
      { name: "voided_at", var_type: "Option<Timestamp>", initial: "null" },
    ],
  },
  Order: {
    entity_type: "Order",
    states: ["Pending", "Confirmed", "Shipped", "Delivered", "Returned"],
    initial_state: "Pending",
    actions: [
      { name: "confirm", kind: "input", from: ["Pending"], to: "Confirmed", guards: ["items.len() > 0"], effects: ["set confirmed_at"] },
      { name: "ship", kind: "input", from: ["Confirmed"], to: "Shipped", guards: ["shipping_address != null"], effects: ["set shipped_at", "generate tracking_id"] },
      { name: "deliver", kind: "input", from: ["Shipped"], to: "Delivered", guards: [], effects: ["set delivered_at"] },
      { name: "return_order", kind: "input", from: ["Delivered"], to: "Returned", guards: ["days_since_delivered < 14"], effects: ["set returned_at"] },
      { name: "cancel", kind: "input", from: ["Pending"], to: "Pending", guards: [], effects: ["set cancelled_at"] },
    ],
    invariants: [
      { name: "shipped_has_tracking", when: ["Shipped"], assertion: "tracking_id != null" },
      { name: "no_further_transitions", when: ["Returned"], assertion: "no outgoing transitions" },
    ],
    state_variables: [
      { name: "shipping_address", var_type: "Option<Address>", initial: "null" },
      { name: "tracking_id", var_type: "Option<String>", initial: "null" },
      { name: "confirmed_at", var_type: "Option<Timestamp>", initial: "null" },
      { name: "shipped_at", var_type: "Option<Timestamp>", initial: "null" },
      { name: "delivered_at", var_type: "Option<Timestamp>", initial: "null" },
      { name: "returned_at", var_type: "Option<Timestamp>", initial: "null" },
    ],
  },
};

// --- Mock Entities ---

export const MOCK_ENTITIES: EntitySummary[] = [
  { entity_type: "Ticket", entity_id: "TKT-001", actor_status: "active", current_state: "InProgress" },
  { entity_type: "Ticket", entity_id: "TKT-002", actor_status: "active", current_state: "Open" },
  { entity_type: "Ticket", entity_id: "TKT-003", actor_status: "active", current_state: "Review" },
  { entity_type: "Ticket", entity_id: "TKT-004", actor_status: "active", current_state: "Closed" },
  { entity_type: "Invoice", entity_id: "INV-001", actor_status: "active", current_state: "Sent" },
  { entity_type: "Invoice", entity_id: "INV-002", actor_status: "active", current_state: "Paid" },
  { entity_type: "Invoice", entity_id: "INV-003", actor_status: "active", current_state: "Draft" },
  { entity_type: "Order", entity_id: "ORD-001", actor_status: "active", current_state: "Shipped" },
  { entity_type: "Order", entity_id: "ORD-002", actor_status: "active", current_state: "Pending" },
];

// --- Mock Verification Results ---

export const MOCK_VERIFICATION: Record<string, VerificationResult> = {
  Ticket: {
    all_passed: true,
    levels: [
      { level: "L0: SMT", passed: true, summary: "All 8 guard expressions are satisfiable", duration_ms: 12 },
      { level: "L1: Model Check", passed: true, summary: "Exhaustive exploration: 47 states, 156 transitions, no dead guards, no unreachable states", duration_ms: 340 },
      { level: "L2: DST", passed: true, summary: "500 simulation ticks with fault injection, all invariants held", duration_ms: 1200 },
      { level: "L2b: Actor Sim", passed: true, summary: "Multi-actor simulation with 3 concurrent tickets, no conflicts", duration_ms: 2100 },
      { level: "L3: PropTest", passed: true, summary: "10,000 random input sequences, no invariant violations", duration_ms: 4500 },
    ],
  },
  Invoice: {
    all_passed: false,
    levels: [
      { level: "L0: SMT", passed: true, summary: "All 6 guard expressions are satisfiable", duration_ms: 8 },
      { level: "L1: Model Check", passed: true, summary: "Exhaustive exploration: 32 states, 89 transitions", duration_ms: 210 },
      { level: "L2: DST", passed: false, summary: "Invariant violation at tick 312: paid_amount != total when state == Paid after concurrent payment", duration_ms: 980, details: "Scenario: Two concurrent record_payment actions arrive for the same invoice. The second payment overwrites paid_amount instead of being rejected by the guard. The invariant paid_has_amount fails because paid_amount reflects only the second payment.\n\nSeed: 42, Tick: 312\nState at violation: { state: Paid, total: 500.00, paid_amount: 250.00 }" },
      { level: "L2b: Actor Sim", passed: false, summary: "Skipped (L2 failed)", duration_ms: 0 },
      { level: "L3: PropTest", passed: false, summary: "Skipped (L2 failed)", duration_ms: 0 },
    ],
  },
  Order: {
    all_passed: true,
    levels: [
      { level: "L0: SMT", passed: true, summary: "All 4 guard expressions are satisfiable", duration_ms: 6 },
      { level: "L1: Model Check", passed: true, summary: "Exhaustive exploration: 28 states, 72 transitions", duration_ms: 180 },
      { level: "L2: DST", passed: true, summary: "500 simulation ticks, all invariants held", duration_ms: 900 },
      { level: "L2b: Actor Sim", passed: true, summary: "Multi-actor simulation passed", duration_ms: 1800 },
      { level: "L3: PropTest", passed: true, summary: "10,000 random sequences, no violations", duration_ms: 3200 },
    ],
  },
};

// --- Mock Entity History ---

export const MOCK_ENTITY_HISTORY: Record<string, EntityHistory> = {
  "Ticket:TKT-001": {
    entity_type: "Ticket",
    entity_id: "TKT-001",
    current_state: "InProgress",
    events: [
      { timestamp: "2026-02-10T09:00:00Z", action: "create", from_state: "(init)", to_state: "Open", actor: "system" },
      { timestamp: "2026-02-10T09:15:00Z", action: "assign", from_state: "Open", to_state: "Open", actor: "alice@example.com" },
      { timestamp: "2026-02-10T10:30:00Z", action: "start_work", from_state: "Open", to_state: "InProgress", actor: "bob@example.com" },
    ],
  },
  "Ticket:TKT-003": {
    entity_type: "Ticket",
    entity_id: "TKT-003",
    current_state: "Review",
    events: [
      { timestamp: "2026-02-09T14:00:00Z", action: "create", from_state: "(init)", to_state: "Open", actor: "system" },
      { timestamp: "2026-02-09T14:05:00Z", action: "assign", from_state: "Open", to_state: "Open", actor: "charlie@example.com" },
      { timestamp: "2026-02-09T16:00:00Z", action: "start_work", from_state: "Open", to_state: "InProgress", actor: "charlie@example.com" },
      { timestamp: "2026-02-10T11:00:00Z", action: "submit_review", from_state: "InProgress", to_state: "Review", actor: "charlie@example.com" },
    ],
  },
  "Invoice:INV-001": {
    entity_type: "Invoice",
    entity_id: "INV-001",
    current_state: "Sent",
    events: [
      { timestamp: "2026-02-08T08:00:00Z", action: "create", from_state: "(init)", to_state: "Draft", actor: "system" },
      { timestamp: "2026-02-08T08:30:00Z", action: "finalize", from_state: "Draft", to_state: "Draft", actor: "finance@example.com" },
      { timestamp: "2026-02-08T09:00:00Z", action: "send", from_state: "Draft", to_state: "Sent", actor: "finance@example.com" },
    ],
  },
  "Order:ORD-001": {
    entity_type: "Order",
    entity_id: "ORD-001",
    current_state: "Shipped",
    events: [
      { timestamp: "2026-02-07T10:00:00Z", action: "create", from_state: "(init)", to_state: "Pending", actor: "system" },
      { timestamp: "2026-02-07T10:05:00Z", action: "confirm", from_state: "Pending", to_state: "Confirmed", actor: "warehouse@example.com" },
      { timestamp: "2026-02-08T14:00:00Z", action: "ship", from_state: "Confirmed", to_state: "Shipped", actor: "logistics@example.com" },
    ],
  },
};
