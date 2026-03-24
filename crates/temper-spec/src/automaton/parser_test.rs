pub(super) const ORDER_IOA: &str = include_str!("../../../../test-fixtures/specs/order.ioa.toml");

#[path = "parser_test_core.rs"]
mod core;
#[path = "parser_test_features.rs"]
mod features;
#[path = "parser_test_integrations.rs"]
mod integrations;
#[path = "parser_test_triggers.rs"]
mod triggers;
