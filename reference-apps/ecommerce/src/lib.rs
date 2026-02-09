//! Reference e-commerce application for the Temper platform.
//!
//! This crate provides spec constants and shared utilities for the
//! verification tests. The specs themselves live in the `specs/` directory.

/// Order entity IOA specification.
pub const ORDER_IOA: &str = include_str!("../specs/order.ioa.toml");

/// Payment entity IOA specification.
pub const PAYMENT_IOA: &str = include_str!("../specs/payment.ioa.toml");

/// Shipment entity IOA specification.
pub const SHIPMENT_IOA: &str = include_str!("../specs/shipment.ioa.toml");

/// CSDL data model.
pub const MODEL_CSDL: &str = include_str!("../specs/model.csdl.xml");

/// Cedar policies for Order entity.
pub const ORDER_CEDAR: &str = include_str!("../specs/policies/order.cedar");
