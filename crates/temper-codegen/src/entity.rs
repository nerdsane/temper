//! Generate entity state structs from CSDL EntityType definitions.

use temper_spec::csdl::{EntityType, Property};

/// Generate a Rust struct definition for an entity's state.
pub fn generate_entity_struct(entity: &EntityType, namespace: &str) -> String {
    let mut out = String::new();

    out.push_str(&format!(
        "/// Entity state for {} (generated from CSDL).\n",
        entity.name
    ));
    out.push_str("#[derive(Debug, Clone, Serialize, Deserialize)]\n");
    out.push_str(&format!("pub struct {}State {{\n", entity.name));

    for prop in &entity.properties {
        let rust_type = csdl_type_to_rust(&prop.type_name, prop.nullable, namespace);
        out.push_str(&format!("    pub {}: {},\n", to_snake_case(&prop.name), rust_type));
    }

    out.push_str("}\n");
    out
}

/// Generate a Default impl that sets initial state machine state.
pub fn generate_entity_default(entity: &EntityType) -> String {
    let initial_state = entity.initial_state().unwrap_or_else(|| "Draft".to_string());
    let mut out = String::new();

    out.push_str(&format!("impl Default for {}State {{\n", entity.name));
    out.push_str("    fn default() -> Self {\n");
    out.push_str("        Self {\n");

    for prop in &entity.properties {
        let default_val = property_default(prop, &initial_state);
        out.push_str(&format!(
            "            {}: {},\n",
            to_snake_case(&prop.name),
            default_val
        ));
    }

    out.push_str("        }\n");
    out.push_str("    }\n");
    out.push_str("}\n");
    out
}

fn property_default(prop: &Property, initial_state: &str) -> String {
    // If this is the Status field, use the initial state
    if (prop.name == "Status" || prop.name.ends_with("Status"))
        && prop.type_name.contains("Status")
    {
        return format!("{}Status::{}", extract_enum_prefix(&prop.type_name), initial_state);
    }

    if let Some(ref default) = prop.default_value {
        match prop.type_name.as_str() {
            "Edm.String" => format!("\"{}\".to_string()", default),
            "Edm.Boolean" => default.to_string(),
            "Edm.Int32" | "Edm.Int64" => default.to_string(),
            _ if prop.type_name.contains("Decimal") => "Decimal::ZERO".to_string(),
            _ => "Default::default()".to_string(),
        }
    } else if prop.nullable {
        "None".to_string()
    } else {
        match prop.type_name.as_str() {
            "Edm.Guid" => "Uuid::now_v7()".to_string(),
            "Edm.String" => "String::new()".to_string(),
            "Edm.Boolean" => "false".to_string(),
            "Edm.Int32" => "0".to_string(),
            "Edm.Int64" => "0".to_string(),
            "Edm.DateTimeOffset" => "Utc::now()".to_string(),
            _ if prop.type_name.contains("Decimal") => "Decimal::ZERO".to_string(),
            _ => "Default::default()".to_string(),
        }
    }
}

fn extract_enum_prefix(type_name: &str) -> &str {
    // "Temper.Example.OrderStatus" → "Order"
    let name = type_name.rsplit('.').next().unwrap_or(type_name);
    name.strip_suffix("Status").unwrap_or(name)
}

/// Convert an OData CSDL type to a Rust type.
pub fn csdl_type_to_rust(type_name: &str, nullable: bool, namespace: &str) -> String {
    let is_collection = type_name.starts_with("Collection(");
    let inner = if is_collection {
        type_name
            .strip_prefix("Collection(")
            .and_then(|s| s.strip_suffix(')'))
            .unwrap_or(type_name)
    } else {
        type_name
    };

    let rust_type = match inner {
        "Edm.Guid" => "Uuid".to_string(),
        "Edm.String" => "String".to_string(),
        "Edm.Boolean" => "bool".to_string(),
        "Edm.Int32" => "i32".to_string(),
        "Edm.Int64" => "i64".to_string(),
        "Edm.Double" => "f64".to_string(),
        "Edm.DateTimeOffset" => "DateTime<Utc>".to_string(),
        _ if inner.contains("Decimal") => "Decimal".to_string(),
        _ => {
            // Strip namespace prefix for local types
            let short = inner.strip_prefix(&format!("{namespace}.")).unwrap_or(inner);
            short.rsplit('.').next().unwrap_or(short).to_string()
        }
    };

    if is_collection {
        format!("Vec<{}>", rust_type)
    } else if nullable {
        format!("Option<{}>", rust_type)
    } else {
        rust_type
    }
}

/// Convert PascalCase to snake_case.
pub fn to_snake_case(s: &str) -> String {
    let mut result = String::new();
    for (i, ch) in s.chars().enumerate() {
        if ch.is_uppercase() && i > 0 {
            // Don't insert underscore between consecutive uppercase (e.g., "ID" -> "id")
            let prev_upper = s.chars().nth(i - 1).is_some_and(|c| c.is_uppercase());
            let next_lower = s.chars().nth(i + 1).is_some_and(|c| c.is_lowercase());
            if !prev_upper || next_lower {
                result.push('_');
            }
        }
        result.push(ch.to_lowercase().next().unwrap_or(ch));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_to_snake_case() {
        assert_eq!(to_snake_case("OrderStatus"), "order_status");
        assert_eq!(to_snake_case("Id"), "id");
        assert_eq!(to_snake_case("CreatedAt"), "created_at");
        assert_eq!(to_snake_case("CustomerId"), "customer_id");
        assert_eq!(to_snake_case("ShippingAddressId"), "shipping_address_id");
    }

    #[test]
    fn test_csdl_type_to_rust() {
        assert_eq!(csdl_type_to_rust("Edm.Guid", false, "Ns"), "Uuid");
        assert_eq!(csdl_type_to_rust("Edm.String", true, "Ns"), "Option<String>");
        assert_eq!(csdl_type_to_rust("Edm.Int32", false, "Ns"), "i32");
        assert_eq!(
            csdl_type_to_rust("Collection(Edm.Guid)", false, "Ns"),
            "Vec<Uuid>"
        );
        assert_eq!(
            csdl_type_to_rust("Temper.Example.OrderStatus", false, "Temper.Example"),
            "OrderStatus"
        );
    }
}
