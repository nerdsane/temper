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

#[cfg(test)]
mod tests {
    use super::*;
    use temper_spec::csdl::{Action, Function, Parameter};

    /// Helper to create a binding parameter (filtered out by action_params/function_params).
    fn binding_param(entity_type: &str) -> Parameter {
        Parameter {
            name: "bindingParameter".to_string(),
            type_name: entity_type.to_string(),
            nullable: false,
            default_value: None,
        }
    }

    /// Helper to create a non-binding parameter.
    fn param(name: &str, type_name: &str, nullable: bool) -> Parameter {
        Parameter {
            name: name.to_string(),
            type_name: type_name.to_string(),
            nullable,
            default_value: None,
        }
    }

    /// Helper to create a bound action with the given parameters.
    fn make_action(name: &str, params: Vec<Parameter>) -> Action {
        Action {
            name: name.to_string(),
            is_bound: true,
            parameters: params,
            return_type: None,
            annotations: vec![],
        }
    }

    /// Helper to create a bound function with the given parameters.
    fn make_function(name: &str, params: Vec<Parameter>) -> Function {
        Function {
            name: name.to_string(),
            is_bound: true,
            parameters: params,
            return_type: None,
            annotations: vec![],
        }
    }

    #[test]
    fn empty_actions_and_functions() {
        let result = generate_message_enum("Order", &[], &[], "Ns");

        assert!(
            result.contains("pub enum OrderMsg {"),
            "expected enum declaration"
        );
        assert!(result.contains("GetState"), "expected GetState variant");
        assert!(
            result.contains("impl temper_runtime::actor::Message for OrderMsg"),
            "expected Message trait impl"
        );
    }

    #[test]
    fn action_no_params() {
        let action = make_action("Submit", vec![binding_param("Ns.Order")]);
        let result = generate_message_enum("Order", &[&action], &[], "Ns");

        // Should generate a unit variant (no braces around fields)
        assert!(
            result.contains("/// Action: Submit"),
            "expected action doc comment"
        );
        assert!(
            result.contains("    Submit,"),
            "expected unit variant Submit"
        );
        assert!(result.contains("GetState"), "expected GetState variant");
    }

    #[test]
    fn action_with_params() {
        let action = make_action(
            "AssignTo",
            vec![
                binding_param("Ns.Order"),
                param("UserId", "Edm.Guid", false),
                param("Notes", "Edm.String", true),
            ],
        );
        let result = generate_message_enum("Order", &[&action], &[], "Ns");

        // Should generate a struct variant
        assert!(
            result.contains("/// Action: AssignTo"),
            "expected action doc comment"
        );
        assert!(
            result.contains("AssignTo {"),
            "expected struct variant opening"
        );
        assert!(
            result.contains("user_id: Uuid"),
            "expected snake_case Uuid field"
        );
        assert!(
            result.contains("notes: Option<String>"),
            "expected nullable string as Option<String>"
        );
    }

    #[test]
    fn function_with_params() {
        let func = make_function(
            "GetSummary",
            vec![
                binding_param("Ns.Order"),
                param("IncludeDetails", "Edm.Boolean", false),
            ],
        );
        let result = generate_message_enum("Order", &[], &[&func], "Ns");

        // Should generate a struct variant with read-only doc comment
        assert!(
            result.contains("/// Function: GetSummary (read-only)"),
            "expected function doc comment with read-only marker"
        );
        assert!(
            result.contains("GetSummary {"),
            "expected struct variant opening"
        );
        assert!(
            result.contains("include_details: bool"),
            "expected snake_case bool field"
        );
    }

    #[test]
    fn multiple_actions_and_functions() {
        let action1 = make_action("Activate", vec![binding_param("Ns.Order")]);
        let action2 = make_action(
            "SetPriority",
            vec![
                binding_param("Ns.Order"),
                param("Level", "Edm.Int32", false),
            ],
        );
        let func1 = make_function(
            "ComputeTotal",
            vec![
                binding_param("Ns.Order"),
                param("TaxRate", "Edm.Decimal", false),
            ],
        );
        let func2 = make_function("GetHistory", vec![binding_param("Ns.Order")]);

        let result = generate_message_enum("Order", &[&action1, &action2], &[&func1, &func2], "Ns");

        // Action variants
        assert!(
            result.contains("    Activate,"),
            "expected unit variant Activate"
        );
        assert!(
            result.contains("SetPriority {"),
            "expected struct variant SetPriority"
        );
        assert!(
            result.contains("level: i32"),
            "expected i32 field for Level"
        );

        // Function variants
        assert!(
            result.contains("/// Function: ComputeTotal (read-only)"),
            "expected ComputeTotal doc comment"
        );
        assert!(
            result.contains("ComputeTotal {"),
            "expected struct variant ComputeTotal"
        );
        assert!(
            result.contains("/// Function: GetHistory (read-only)"),
            "expected GetHistory doc comment"
        );
        assert!(
            result.contains("    GetHistory,"),
            "expected unit variant GetHistory"
        );

        // Always present
        assert!(result.contains("GetState"), "expected GetState variant");
    }
}
