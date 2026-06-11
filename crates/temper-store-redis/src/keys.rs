//! Redis key naming conventions for Temper.
//!
//! All keys are prefixed with `temper:` to namespace within shared Redis
//! instances. Subsystem-specific key builders live next to their subsystem;
//! this module holds only the shared prefix.

/// Key prefix for all Temper Redis keys.
pub const PREFIX: &str = "temper";
