//! Project management OS app for the Temper platform.
//!
//! Bundled OS app for agents to track engineering work: issues, projects,
//! cycles, comments, and labels. Distinct from platform Plan/Task entities
//! which track agent execution — this tracks product/engineering work items.

/// Issue entity IOA specification.
pub const ISSUE_IOA: &str = include_str!("../specs/issue.ioa.toml");

/// Project entity IOA specification.
pub const PROJECT_IOA: &str = include_str!("../specs/project.ioa.toml");

/// Cycle entity IOA specification.
pub const CYCLE_IOA: &str = include_str!("../specs/cycle.ioa.toml");

/// Comment entity IOA specification.
pub const COMMENT_IOA: &str = include_str!("../specs/comment.ioa.toml");

/// Label entity IOA specification.
pub const LABEL_IOA: &str = include_str!("../specs/label.ioa.toml");

/// CSDL data model.
pub const MODEL_CSDL: &str = include_str!("../specs/model.csdl.xml");
