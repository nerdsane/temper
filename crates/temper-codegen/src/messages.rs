//! Generate message enums from CSDL Actions and Functions.

use temper_spec::csdl::{Action, Function};

use crate::entity::{csdl_type_to_rust, to_snake_case};

/// Generate the message enum for an entity's actor.
/// Each action becomes a command variant, each function becomes a query variant.
pub fn generate_message_enum(
    entity_name: &str,
    actions: &[&Action],
    functions: &[&Function],
    namespace: &str,
) -> String {
    let mut out = String::new();

    out.push_str(&format!(
        "/// Messages for the {} actor (generated from CSDL actions/functions).\n",
        entity_name
    ));
    out.push_str("#[derive(Debug)]\n");
    out.push_str(&format!("pub enum {}Msg {{\n", entity_name));

    // Actions → command variants
    for action in actions {
        let params = action_params(action, namespace);
        if params.is_empty() {
            out.push_str(&format!("    /// Action: {}\n", action.name));
            out.push_str(&format!("    {},\n", action.name));
        } else {
            out.push_str(&format!("    /// Action: {}\n", action.name));
            out.push_str(&format!("    {} {{\n", action.name));
            for (name, ty) in &params {
                out.push_str(&format!("        {}: {},\n", name, ty));
            }
            out.push_str("    },\n");
        }
    }

    // Functions → query variants
    for func in functions {
        let params = function_params(func, namespace);
        if params.is_empty() {
            out.push_str(&format!("    /// Function: {} (read-only)\n", func.name));
            out.push_str(&format!("    {},\n", func.name));
        } else {
            out.push_str(&format!("    /// Function: {} (read-only)\n", func.name));
            out.push_str(&format!("    {} {{\n", func.name));
            for (name, ty) in &params {
                out.push_str(&format!("        {}: {},\n", name, ty));
            }
            out.push_str("    },\n");
        }
    }

    // Standard CRUD operations
    out.push_str("    /// Get current entity state\n");
    out.push_str("    GetState,\n");

    out.push_str("}\n\n");

    // Implement Message trait
    out.push_str(&format!(
        "impl temper_runtime::actor::Message for {}Msg {{}}\n",
        entity_name
    ));

    out
}

/// Extract non-binding parameters from an action.
fn action_params(action: &Action, namespace: &str) -> Vec<(String, String)> {
    action
        .parameters
        .iter()
        .filter(|p| p.name != "bindingParameter")
        .map(|p| {
            let name = to_snake_case(&p.name);
            let ty = csdl_type_to_rust(&p.type_name, p.nullable, namespace);
            (name, ty)
        })
        .collect()
}

/// Extract non-binding parameters from a function.
fn function_params(func: &Function, namespace: &str) -> Vec<(String, String)> {
    func.parameters
        .iter()
        .filter(|p| p.name != "bindingParameter")
        .map(|p| {
            let name = to_snake_case(&p.name);
            let ty = csdl_type_to_rust(&p.type_name, p.nullable, namespace);
            (name, ty)
        })
        .collect()
}
