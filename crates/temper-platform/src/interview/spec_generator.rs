//! Spec generators that produce IOA TOML, CSDL XML, and Cedar policies
//! from collected [`EntityModel`] data.

use super::entity_collector::{ActionKind, EntityModel};

/// Generate an IOA TOML specification from an entity model.
///
/// The output is valid I/O Automaton TOML parseable by
/// `temper_spec::automaton::parse_automaton()`.
pub fn generate_ioa_toml(entity: &EntityModel) -> String {
    let mut out = String::new();

    // Header comment
    out.push_str(&format!(
        "# {} Entity -- I/O Automaton Specification\n",
        entity.name
    ));
    if !entity.description.is_empty() {
        out.push_str(&format!("# {}\n", entity.description));
    }
    out.push('\n');

    // [automaton] section
    out.push_str("[automaton]\n");
    out.push_str(&format!("name = \"{}\"\n", entity.name));

    let state_names: Vec<String> = entity
        .states
        .iter()
        .map(|s| format!("\"{}\"", s.name))
        .collect();
    out.push_str(&format!("states = [{}]\n", state_names.join(", ")));

    let initial = entity
        .initial_state()
        .unwrap_or_else(|| entity.states.first().map(|s| s.name.as_str()).unwrap_or(""));
    out.push_str(&format!("initial = \"{initial}\"\n"));

    // [[state]] sections for state variables
    if !entity.state_variables.is_empty() {
        out.push_str("\n# --- State Variables ---\n");
        for var in &entity.state_variables {
            out.push('\n');
            out.push_str("[[state]]\n");
            out.push_str(&format!("name = \"{}\"\n", var.name));
            out.push_str(&format!("type = \"{}\"\n", var.var_type));
            out.push_str(&format!("initial = \"{}\"\n", var.initial));
        }
    }

    // [[action]] sections
    if !entity.actions.is_empty() {
        out.push_str("\n# --- Actions ---\n");
        for action in &entity.actions {
            out.push('\n');
            out.push_str("[[action]]\n");
            out.push_str(&format!("name = \"{}\"\n", action.name));
            out.push_str(&format!("kind = \"{}\"\n", action.kind.as_str()));

            if !action.from_states.is_empty() {
                let from: Vec<String> =
                    action.from_states.iter().map(|s| format!("\"{s}\"")).collect();
                out.push_str(&format!("from = [{}]\n", from.join(", ")));
            }

            if let Some(ref to) = action.to_state {
                out.push_str(&format!("to = \"{to}\"\n"));
            }

            if let Some(ref guard_expr) = action.guard {
                out.push_str(&format!("guard = \"{guard_expr}\"\n"));
            }

            if !action.params.is_empty() {
                let params: Vec<String> =
                    action.params.iter().map(|p| format!("\"{p}\"")).collect();
                out.push_str(&format!("params = [{}]\n", params.join(", ")));
            }

            if let Some(ref hint) = action.hint {
                out.push_str(&format!("hint = \"{hint}\"\n"));
            }
        }
    }

    // [[invariant]] sections
    if !entity.invariants.is_empty() {
        out.push_str("\n# --- Safety Invariants ---\n");
        for inv in &entity.invariants {
            out.push('\n');
            out.push_str("[[invariant]]\n");
            out.push_str(&format!("name = \"{}\"\n", inv.name));
            if !inv.when.is_empty() {
                let when: Vec<String> = inv.when.iter().map(|s| format!("\"{s}\"")).collect();
                out.push_str(&format!("when = [{}]\n", when.join(", ")));
            }
            out.push_str(&format!("assert = \"{}\"\n", inv.assertion));
        }
    }

    out
}

/// Generate OData CSDL XML from a list of entity models.
///
/// Produces a valid edmx:Edmx document with entity types, an entity container,
/// and Temper.Vocab annotations on actions.
pub fn generate_csdl_xml(entities: &[EntityModel], namespace: &str) -> String {
    let mut out = String::new();

    out.push_str("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n");
    out.push_str("<edmx:Edmx Version=\"4.0\" xmlns:edmx=\"http://docs.oasis-open.org/odata/ns/edmx\">\n");
    out.push_str("  <edmx:DataServices>\n");
    out.push_str(&format!(
        "    <Schema Namespace=\"{namespace}\" xmlns=\"http://docs.oasis-open.org/odata/ns/edm\">\n"
    ));

    // Entity types
    for entity in entities {
        out.push_str(&format!("      <EntityType Name=\"{}\">\n", entity.name));
        out.push_str("        <Key>\n");
        out.push_str("          <PropertyRef Name=\"Id\" />\n");
        out.push_str("        </Key>\n");
        out.push_str(
            "        <Property Name=\"Id\" Type=\"Edm.Guid\" Nullable=\"false\" />\n",
        );
        out.push_str(
            "        <Property Name=\"Status\" Type=\"Edm.String\" Nullable=\"false\" />\n",
        );

        // State variables as properties
        for var in &entity.state_variables {
            let edm_type = match var.var_type.as_str() {
                "counter" => "Edm.Int32",
                "bool" => "Edm.Boolean",
                "string" => "Edm.String",
                "set" => "Collection(Edm.String)",
                _ => "Edm.String",
            };
            out.push_str(&format!(
                "        <Property Name=\"{}\" Type=\"{}\" />\n",
                pascal_case(&var.name),
                edm_type
            ));
        }

        out.push_str("      </EntityType>\n");
    }

    // Actions
    for entity in entities {
        for action in &entity.actions {
            if action.kind == ActionKind::Output {
                continue; // Output actions are events, not OData actions
            }
            out.push_str(&format!(
                "      <Action Name=\"{}\" IsBound=\"true\">\n",
                action.name
            ));
            out.push_str(&format!(
                "        <Parameter Name=\"bindingParameter\" Type=\"{namespace}.{}\" />\n",
                entity.name
            ));
            for param in &action.params {
                out.push_str(&format!(
                    "        <Parameter Name=\"{param}\" Type=\"Edm.String\" />\n"
                ));
            }
            out.push_str("        <Annotation Term=\"Temper.Vocab.ActionKind\" ");
            out.push_str(&format!("String=\"{}\" />\n", action.kind.as_str()));
            out.push_str("      </Action>\n");
        }
    }

    // Entity container
    out.push_str("      <EntityContainer Name=\"Container\">\n");
    for entity in entities {
        out.push_str(&format!(
            "        <EntitySet Name=\"{}s\" EntityType=\"{namespace}.{}\" />\n",
            entity.name, entity.name
        ));
    }
    out.push_str("      </EntityContainer>\n");

    out.push_str("    </Schema>\n");
    out.push_str("  </edmx:DataServices>\n");
    out.push_str("</edmx:Edmx>\n");

    out
}

/// Generate Cedar authorization policies for an entity model.
///
/// Produces basic ABAC policies: one permit per action, plus a default deny.
pub fn generate_cedar_policies(entity: &EntityModel) -> String {
    let mut out = String::new();

    out.push_str(&format!(
        "// Cedar policies for {} entity\n\n",
        entity.name
    ));

    for action in &entity.actions {
        if action.kind == ActionKind::Output {
            continue; // Output actions are not user-invokable
        }
        out.push_str(&format!(
            "// Allow {} action\n",
            action.name
        ));
        out.push_str(&format!(
            "permit (\n  principal,\n  action == Action::\"{}\",\n  resource\n);\n\n",
            action.name
        ));
    }

    // Default deny
    out.push_str("// Default deny for unmatched actions\n");
    out.push_str("forbid (\n  principal,\n  action,\n  resource\n);\n");

    out
}

/// Convert a snake_case name to PascalCase.
fn pascal_case(name: &str) -> String {
    name.split('_')
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(c) => {
                    let upper: String = c.to_uppercase().collect();
                    format!("{upper}{}", chars.as_str())
                }
                None => String::new(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interview::entity_collector::*;

    fn make_test_entity() -> EntityModel {
        EntityModel {
            name: "Task".to_string(),
            description: "A project management task".to_string(),
            states: vec![
                StateDefinition {
                    name: "Open".to_string(),
                    description: "Newly created".to_string(),
                    is_terminal: false,
                },
                StateDefinition {
                    name: "InProgress".to_string(),
                    description: "Being worked on".to_string(),
                    is_terminal: false,
                },
                StateDefinition {
                    name: "Done".to_string(),
                    description: "Completed".to_string(),
                    is_terminal: true,
                },
            ],
            actions: vec![
                ActionDefinition {
                    name: "StartTask".to_string(),
                    from_states: vec!["Open".to_string()],
                    to_state: Some("InProgress".to_string()),
                    guard: None,
                    params: vec!["AssigneeId".to_string()],
                    hint: Some("Begin working on the task".to_string()),
                    kind: ActionKind::Internal,
                },
                ActionDefinition {
                    name: "CompleteTask".to_string(),
                    from_states: vec!["InProgress".to_string()],
                    to_state: Some("Done".to_string()),
                    guard: None,
                    params: vec![],
                    hint: Some("Mark the task as done".to_string()),
                    kind: ActionKind::Internal,
                },
                ActionDefinition {
                    name: "TaskCompletedEvent".to_string(),
                    from_states: vec![],
                    to_state: None,
                    guard: None,
                    params: vec![],
                    hint: Some("Emitted on completion".to_string()),
                    kind: ActionKind::Output,
                },
            ],
            invariants: vec![],
            state_variables: vec![],
        }
    }

    fn make_order_entity() -> EntityModel {
        EntityModel {
            name: "Order".to_string(),
            description: "An e-commerce order".to_string(),
            states: vec![
                StateDefinition {
                    name: "Draft".to_string(),
                    description: "Initial".to_string(),
                    is_terminal: false,
                },
                StateDefinition {
                    name: "Submitted".to_string(),
                    description: "Submitted".to_string(),
                    is_terminal: false,
                },
                StateDefinition {
                    name: "Cancelled".to_string(),
                    description: "Cancelled".to_string(),
                    is_terminal: true,
                },
            ],
            actions: vec![
                ActionDefinition {
                    name: "AddItem".to_string(),
                    from_states: vec!["Draft".to_string()],
                    to_state: None,
                    guard: None,
                    params: vec!["ProductId".to_string(), "Quantity".to_string()],
                    hint: Some("Add an item".to_string()),
                    kind: ActionKind::Input,
                },
                ActionDefinition {
                    name: "SubmitOrder".to_string(),
                    from_states: vec!["Draft".to_string()],
                    to_state: Some("Submitted".to_string()),
                    guard: Some("items > 0".to_string()),
                    params: vec![],
                    hint: Some("Submit the order".to_string()),
                    kind: ActionKind::Internal,
                },
            ],
            invariants: vec![InvariantDefinition {
                name: "SubmitRequiresItems".to_string(),
                when: vec!["Submitted".to_string()],
                assertion: "items > 0".to_string(),
            }],
            state_variables: vec![StateVariable {
                name: "items".to_string(),
                var_type: "counter".to_string(),
                initial: "0".to_string(),
            }],
        }
    }

    #[test]
    fn test_generate_ioa_toml_parses() {
        let entity = make_order_entity();
        let toml = generate_ioa_toml(&entity);
        let result = temper_spec::automaton::parse_automaton(&toml);
        assert!(
            result.is_ok(),
            "Generated TOML should parse. Got error: {:?}\nTOML:\n{toml}",
            result.err()
        );
    }

    #[test]
    fn test_generate_ioa_toml_roundtrip() {
        let entity = make_order_entity();
        let toml = generate_ioa_toml(&entity);
        let automaton = temper_spec::automaton::parse_automaton(&toml).unwrap();
        assert_eq!(automaton.automaton.name, "Order");
        assert_eq!(automaton.automaton.initial, "Draft");
        assert_eq!(automaton.automaton.states.len(), 3);
        assert_eq!(automaton.actions.len(), 2);
        assert_eq!(automaton.invariants.len(), 1);
        assert_eq!(automaton.state.len(), 1);
        assert_eq!(automaton.state[0].name, "items");
    }

    #[test]
    fn test_generate_ioa_toml_structure() {
        let entity = make_test_entity();
        let toml = generate_ioa_toml(&entity);
        assert!(toml.contains("[automaton]"));
        assert!(toml.contains("name = \"Task\""));
        assert!(toml.contains("initial = \"Open\""));
        assert!(toml.contains("[[action]]"));
        assert!(toml.contains("name = \"StartTask\""));
        // Output actions should be included
        assert!(toml.contains("name = \"TaskCompletedEvent\""));
        assert!(toml.contains("kind = \"output\""));
    }

    #[test]
    fn test_generate_csdl_xml_structure() {
        let entities = vec![make_test_entity()];
        let xml = generate_csdl_xml(&entities, "Temper.TaskTracker");
        assert!(xml.contains("edmx:Edmx"));
        assert!(xml.contains("EntityType Name=\"Task\""));
        assert!(xml.contains("EntitySet Name=\"Tasks\""));
        assert!(xml.contains("Action Name=\"StartTask\""));
        assert!(xml.contains("Namespace=\"Temper.TaskTracker\""));
        // Output actions should NOT appear as OData actions
        assert!(!xml.contains("Action Name=\"TaskCompletedEvent\""));
    }

    #[test]
    fn test_generate_cedar_policies_contains_actions() {
        let entity = make_test_entity();
        let policies = generate_cedar_policies(&entity);
        assert!(policies.contains("Action::\"StartTask\""));
        assert!(policies.contains("Action::\"CompleteTask\""));
        // Output actions should not have Cedar policies
        assert!(!policies.contains("Action::\"TaskCompletedEvent\""));
        // Default deny
        assert!(policies.contains("forbid"));
    }

    #[test]
    fn test_generate_csdl_xml_state_variables() {
        let entities = vec![make_order_entity()];
        let xml = generate_csdl_xml(&entities, "Temper.Ecommerce");
        assert!(xml.contains("Property Name=\"Items\" Type=\"Edm.Int32\""));
    }

    #[test]
    fn test_pascal_case() {
        assert_eq!(pascal_case("has_address"), "HasAddress");
        assert_eq!(pascal_case("items"), "Items");
        assert_eq!(pascal_case("is_active"), "IsActive");
    }
}
