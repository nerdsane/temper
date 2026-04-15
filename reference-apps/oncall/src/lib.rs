//! Reference on-call application for the Temper platform.
//!
//! This crate provides spec constants and shared utilities for the
//! verification tests. Agents are the on-call responders — they get
//! paged, investigate, run remediations, and escalate to more capable
//! agents. Humans are the last-resort tier.

/// Page entity IOA specification.
pub const PAGE_IOA: &str = include_str!("../specs/page.ioa.toml");

/// EscalationPolicy entity IOA specification.
pub const ESCALATION_POLICY_IOA: &str = include_str!("../specs/escalation_policy.ioa.toml");

/// Remediation entity IOA specification.
pub const REMEDIATION_IOA: &str = include_str!("../specs/remediation.ioa.toml");

/// Postmortem entity IOA specification.
pub const POSTMORTEM_IOA: &str = include_str!("../specs/postmortem.ioa.toml");

/// CSDL data model.
pub const MODEL_CSDL: &str = include_str!("../specs/model.csdl.xml");
