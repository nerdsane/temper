//! Inert durable collection-workflow ledger primitives from ADR-0181.
//!
//! This module deliberately does not parse or activate public
//! `[[collection_workflow]]` declarations. It owns only the versioned private
//! evidence and persistence contract consumed by later execution work.

mod identity;
mod lifecycle;
mod model;
mod persistence;
mod validation;

pub(crate) use identity::{
    collection_child_id, collection_control_id, collection_member_id, collection_workflow_id,
};
pub(crate) use model::*;
pub(crate) use persistence::*;

#[cfg(test)]
mod tests;
