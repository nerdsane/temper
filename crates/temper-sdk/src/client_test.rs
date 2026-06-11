//! Tests for the Temper OData client.

use super::*;

use super::*;

#[test]
fn test_builder_defaults() {
    let client = TemperClient::builder()
        .base_url("http://localhost:4200")
        .build()
        .unwrap();
    assert_eq!(client.base_url(), "http://localhost:4200");
    assert_eq!(client.tenant(), "default");
    assert!(client.principal().is_none());
    assert!(client.principal_kind().is_none());
    assert!(client.api_key().is_none());
}

#[test]
fn test_builder_all_fields() {
    let client = TemperClient::builder()
        .base_url("http://localhost:4200/")
        .tenant("acme")
        .principal("agent-1")
        .principal_kind("admin")
        .api_key("secret-key")
        .build()
        .unwrap();
    assert_eq!(client.base_url(), "http://localhost:4200");
    assert_eq!(client.tenant(), "acme");
    assert_eq!(client.principal(), Some("agent-1"));
    assert_eq!(client.principal_kind(), Some("admin"));
    assert_eq!(client.api_key(), Some("secret-key"));
}

#[test]
fn test_builder_requires_base_url() {
    let result = TemperClient::builder().build();
    assert!(result.is_err());
}

#[test]
fn test_new_convenience() {
    let client = TemperClient::new("http://localhost:4200", "default");
    assert_eq!(client.base_url(), "http://localhost:4200");
    assert_eq!(client.tenant(), "default");
}

#[test]
fn test_entity_url() {
    let client = TemperClient::new("http://localhost:4200", "default");
    assert_eq!(
        client.entity_url("Tasks"),
        "http://localhost:4200/tdata/Tasks"
    );
}

#[test]
fn test_entity_instance_url() {
    let client = TemperClient::new("http://localhost:4200", "default");
    assert_eq!(
        client.entity_instance_url("Tasks", "t-1"),
        "http://localhost:4200/tdata/Tasks('t-1')"
    );
}

#[test]
fn test_action_url() {
    let client = TemperClient::new("http://localhost:4200", "default");
    assert_eq!(
        client.action_url("Tasks", "t-1", "Start"),
        "http://localhost:4200/tdata/Tasks('t-1')/Temper.Start"
    );
}

#[test]
fn test_trailing_slash_stripped() {
    let client = TemperClient::new("http://localhost:4200/", "default");
    assert_eq!(client.base_url(), "http://localhost:4200");
    assert_eq!(
        client.entity_url("Agents"),
        "http://localhost:4200/tdata/Agents"
    );
}
