//! Naming collisions between an app's CSDL and the kernel's own fields.

use super::CsdlDocument;

/// Report CSDL properties that collide with server-derived field names.
///
/// `Id`, `id`, `Status`, `status`, `has_spec` and `HasSpec` are owned by the
/// kernel: the actor overwrites them with the entity id and the state-machine
/// state on every hydrate (`canonicalize_entity_field_map`). An app is free to
/// *declare* one in its CSDL — nothing rejects it — but the value it stores is
/// then destroyed on the actor path while the OData projection still reports
/// the declared property, so the same field reads differently depending on
/// which surface you came in through.
///
/// That divergence is silent, and it cost a full day: Genesis declares `Id` on
/// its git objects to hold the bare sha, and the kernel's own bundle lookup
/// compared that field against a sha, got the entity id back, and reported
/// intact commits as "not found" — with nothing anywhere saying why.
///
/// This warns rather than rejects on purpose. Existing apps (Genesis among
/// them) already declare these names, so failing registration would take them
/// down; the fix is to name the collision loudly and let apps migrate.
///
/// Each property is named `Namespace.Entity.Property`. The install that brings
/// a CSDL reports these at warn; registration, which every boot repeats for
/// every stored tenant, logs them only at debug.
pub fn server_derived_csdl_properties(csdl: &CsdlDocument) -> Vec<String> {
    let mut found = Vec::new();
    for schema in &csdl.schemas {
        for entity_type in &schema.entity_types {
            for property in &entity_type.properties {
                if temper_spec::automaton::is_server_derived_field_name(&property.name) {
                    found.push(format!(
                        "{}.{}.{}",
                        schema.namespace, entity_type.name, property.name
                    ));
                }
            }
        }
    }
    found
}

/// Log at debug what [`server_derived_csdl_properties`] finds in a tenant's
/// CSDL as it registers.
pub(super) fn debug_server_derived_csdl_properties(tenant: &str, csdl: &CsdlDocument) {
    for property in server_derived_csdl_properties(csdl) {
        tracing::debug!(
            tenant,
            property,
            "CSDL declares a server-derived field name; the actor will overwrite it"
        );
    }
}
