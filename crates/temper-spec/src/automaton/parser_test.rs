pub(super) const ORDER_IOA: &str = include_str!("../../../../test-fixtures/specs/order.ioa.toml");

#[path = "parser_core_test.rs"]
mod core;
#[path = "parser_features_test.rs"]
mod features;
#[path = "parser_integrations_test.rs"]
mod integrations;
#[path = "parser_triggers_test.rs"]
mod triggers;
