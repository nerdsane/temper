/// Return whether a field name is owned by the runtime rather than an action.
///
/// Entity identity, lifecycle status, spec-governance metadata, and declared
/// context statuses are derived from server-proven state. Specs and callers
/// must not create a second mutable representation of these values.
pub fn is_server_derived_field_name(name: &str) -> bool {
    matches!(
        name,
        "Id" | "id"
            | "Status"
            | "status"
            | "has_spec"
            | "HasSpec"
            | "_temper_state_timeout_declaration_v1"
    ) || is_server_derived_context_status_name(name)
}

/// Return whether a field is in the server-derived context-status namespace.
pub fn is_server_derived_context_status_name(name: &str) -> bool {
    name.starts_with("ctx_") && name.ends_with("_status")
}
