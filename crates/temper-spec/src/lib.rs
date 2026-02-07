//! temper-spec: CSDL and TLA+ specification parser for the Temper framework.
//!
//! This crate provides parsing and representation of entity models defined
//! in CSDL (OData Common Schema Definition Language) and TLA+ specifications.
//!
//! # Usage
//!
//! ```ignore
//! use temper_spec::{parse_csdl, extract_state_machine, build_spec_model};
//! ```

pub mod csdl;
pub mod tlaplus;
pub mod model;

// Re-export primary public API at crate root (Firecracker/DataFusion convention).
pub use csdl::{parse_csdl, CsdlDocument, CsdlParseError};
pub use tlaplus::{extract_state_machine, StateMachine, Transition, Invariant};
pub use model::{build_spec_model, SpecModel};
