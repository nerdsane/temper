//! Factory regression tests.
//!
//! Each module reproduces one scoped finding. Assertions that expose the
//! reported bug carry `FACTORY_REGRESSION_ASSERTION` in their failure message;
//! harness/setup failures use `harness setup:` diagnostics instead.

#[path = "factory_regression/harness.rs"]
mod harness;
#[path = "factory_regression/idempotency/mod.rs"]
mod idempotency;
#[path = "factory_regression/legacy.rs"]
mod legacy;
