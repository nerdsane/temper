//! Shared action-input shape validation for HTTP and module-data adapters.

use std::collections::BTreeMap;

/// Stable action-input shape failure classified by transport adapters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ActionInputShapeError {
    /// A required declaration was absent or explicitly null.
    Missing { parameter: String },
    /// Input names were extra or ambiguous after canonical normalization.
    Mismatch { parameter: String },
}

/// Resolved shape of a non-EDM schema type.
#[derive(Debug, Clone, Copy)]
pub(crate) enum NamedTypeShape<'a> {
    /// A closed enumeration resolved from CSDL metadata.
    CsdlEnum(&'a [temper_spec::csdl::EnumMember]),
    /// A closed enumeration copied into a module SDK manifest.
    ManifestEnum(&'a [String]),
    /// An entity reference represented by its canonical string identifier.
    EntityReference,
    /// A structured CSDL value represented by a JSON object.
    Complex,
}

/// Resolve a named value's exact wire shape from one immutable CSDL document.
pub(crate) fn named_type_shape_from_csdl<'a>(
    csdl: &'a temper_spec::csdl::CsdlDocument,
    type_name: &str,
) -> NamedTypeShape<'a> {
    let type_name = type_name
        .strip_prefix("Collection(")
        .and_then(|name| name.strip_suffix(')'))
        .unwrap_or(type_name);
    let type_tail = type_name.rsplit('.').next().unwrap_or(type_name);
    for schema in &csdl.schemas {
        if let Some(enum_type) = schema.enum_types.iter().find(|candidate| {
            candidate.name == type_tail
                && (type_name == candidate.name
                    || type_name == format!("{}.{}", schema.namespace, candidate.name))
        }) {
            return NamedTypeShape::CsdlEnum(&enum_type.members);
        }
        if schema.entity_types.iter().any(|candidate| {
            candidate.name == type_tail
                && (type_name == candidate.name
                    || type_name == format!("{}.{}", schema.namespace, candidate.name))
        }) {
            return NamedTypeShape::EntityReference;
        }
    }
    NamedTypeShape::Complex
}

/// Validate aliases, extras, presence, and nullability for an action input object.
///
/// Returned values are non-null and keyed by their exact schema declaration so
/// each adapter can apply its schema-specific scalar and enum type checks.
pub(crate) fn validate_action_input_shape<'input, 'schema>(
    input: &'input serde_json::Map<String, serde_json::Value>,
    declarations: impl IntoIterator<Item = (&'schema str, bool)>,
) -> Result<BTreeMap<&'schema str, &'input serde_json::Value>, ActionInputShapeError> {
    let declarations = declarations
        .into_iter()
        .map(|(name, nullable)| (temper_spec::naming::to_snake_case(name), (name, nullable)))
        .collect::<BTreeMap<_, _>>();
    let mut normalized_input = BTreeMap::new();
    for (name, value) in input {
        let normalized = temper_spec::naming::to_snake_case(name);
        if normalized_input.insert(normalized, (name, value)).is_some() {
            return Err(ActionInputShapeError::Mismatch {
                parameter: name.clone(),
            });
        }
    }

    let mut values = BTreeMap::new();
    for (normalized, (canonical, nullable)) in &declarations {
        match normalized_input.get(normalized).map(|(_, value)| *value) {
            None | Some(serde_json::Value::Null) if !nullable => {
                return Err(ActionInputShapeError::Missing {
                    parameter: (*canonical).to_string(),
                });
            }
            None | Some(serde_json::Value::Null) => {}
            Some(value) => {
                values.insert(*canonical, value);
            }
        }
    }
    if let Some((_, (name, _))) = normalized_input
        .iter()
        .find(|(normalized, _)| !declarations.contains_key(*normalized))
    {
        return Err(ActionInputShapeError::Mismatch {
            parameter: (*name).clone(),
        });
    }
    Ok(values)
}

/// Validate one non-null canonical JSON value against CSDL type metadata.
pub(crate) fn value_matches_schema_type(
    value: &serde_json::Value,
    type_name: &str,
    named_type: NamedTypeShape<'_>,
) -> bool {
    if let Some(element_type) = type_name
        .strip_prefix("Collection(")
        .and_then(|name| name.strip_suffix(')'))
    {
        return value.as_array().is_some_and(|values| {
            values
                .iter()
                .all(|value| value_matches_schema_type(value, element_type, named_type))
        });
    }
    match type_name {
        "Edm.Boolean" => value.is_boolean(),
        "Edm.Byte" => integer_in_range(value, 0, u8::MAX as i128),
        "Edm.SByte" => integer_in_range(value, i8::MIN as i128, i8::MAX as i128),
        "Edm.Int16" => integer_in_range(value, i16::MIN as i128, i16::MAX as i128),
        "Edm.Int32" => integer_in_range(value, i32::MIN as i128, i32::MAX as i128),
        "Edm.Int64" => integer_in_range(value, i64::MIN as i128, i64::MAX as i128),
        "Edm.Single" | "Edm.Double" => value.as_f64().is_some_and(f64::is_finite),
        "Edm.Decimal" => value.as_str().is_some_and(decimal_lexical),
        "Edm.Guid" => value
            .as_str()
            .is_some_and(|text| guid_lexical(text) && uuid::Uuid::parse_str(text).is_ok()),
        "Edm.DateTimeOffset" => value
            .as_str()
            .is_some_and(|text| chrono::DateTime::parse_from_rfc3339(text).is_ok()),
        "Edm.Binary" => value.as_str().is_some_and(binary_lexical),
        "Edm.Date" | "Edm.Duration" | "Edm.String" | "Edm.TimeOfDay" => value.is_string(),
        _ => match named_type {
            NamedTypeShape::CsdlEnum(members) => value
                .as_str()
                .is_some_and(|member| members.iter().any(|known| known.name == member)),
            NamedTypeShape::ManifestEnum(members) => value
                .as_str()
                .is_some_and(|member| members.iter().any(|known| known == member)),
            NamedTypeShape::EntityReference => value.is_string(),
            NamedTypeShape::Complex => value.is_object(),
        },
    }
}

fn integer_in_range(value: &serde_json::Value, minimum: i128, maximum: i128) -> bool {
    value
        .as_i64()
        .map(i128::from)
        .or_else(|| value.as_u64().map(i128::from))
        .is_some_and(|value| (minimum..=maximum).contains(&value))
}

fn decimal_lexical(value: &str) -> bool {
    if matches!(value, "NaN" | "INF" | "-INF") {
        return false;
    }
    let unsigned = value.strip_prefix(['+', '-']).unwrap_or(value);
    let exponent_index = unsigned.find(['e', 'E']);
    let (mantissa, exponent) = exponent_index.map_or((unsigned, None), |index| {
        (&unsigned[..index], Some(&unsigned[index + 1..]))
    });
    if exponent.is_some_and(|exponent| {
        let digits = exponent.strip_prefix(['+', '-']).unwrap_or(exponent);
        digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit())
    }) || unsigned.matches(['e', 'E']).count() > 1
    {
        return false;
    }
    let mut parts = mantissa.split('.');
    let whole = parts.next().unwrap_or_default();
    let fraction = parts.next();
    if parts.next().is_some()
        || whole.is_empty()
        || !whole.bytes().all(|byte| byte.is_ascii_digit())
    {
        return false;
    }
    fraction
        .is_none_or(|digits| !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()))
}

fn guid_lexical(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
}

fn binary_lexical(value: &str) -> bool {
    let unpadded = value.trim_end_matches('=');
    let padding = value.len() - unpadded.len();
    if padding > 2
        || unpadded
            .bytes()
            .any(|byte| !(byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')))
    {
        return false;
    }
    match unpadded.len() % 4 {
        0 => padding == 0,
        2 => {
            matches!(padding, 0 | 2)
                && matches!(unpadded.as_bytes().last(), Some(b'A' | b'Q' | b'g' | b'w'))
        }
        3 => {
            padding <= 1
                && matches!(
                    unpadded.as_bytes().last(),
                    Some(
                        b'A' | b'E'
                            | b'I'
                            | b'M'
                            | b'Q'
                            | b'U'
                            | b'Y'
                            | b'c'
                            | b'g'
                            | b'k'
                            | b'o'
                            | b's'
                            | b'w'
                            | b'0'
                            | b'4'
                            | b'8'
                    )
                )
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_type_validation_covers_collections_and_guid_lexical_form() {
        assert!(value_matches_schema_type(
            &serde_json::json!([1, 2]),
            "Collection(Edm.Int16)",
            NamedTypeShape::Complex,
        ));
        assert!(!value_matches_schema_type(
            &serde_json::json!([1, "two"]),
            "Collection(Edm.Int16)",
            NamedTypeShape::Complex,
        ));
        assert!(value_matches_schema_type(
            &serde_json::json!("67e55044-10b1-426f-9247-bb680e5fe0c8"),
            "Edm.Guid",
            NamedTypeShape::Complex,
        ));
        assert!(!value_matches_schema_type(
            &serde_json::json!("not-a-guid"),
            "Edm.Guid",
            NamedTypeShape::Complex,
        ));
        assert!(value_matches_schema_type(
            &serde_json::json!([{"ticket": "writer-1"}]),
            "Collection(Temper.ReadersWriters.WaitRequest)",
            NamedTypeShape::Complex,
        ));
        assert!(!value_matches_schema_type(
            &serde_json::json!(["writer-1"]),
            "Collection(Temper.ReadersWriters.WaitRequest)",
            NamedTypeShape::Complex,
        ));
    }

    #[test]
    fn canonical_type_validation_covers_enums_and_decimal_wire_form() {
        let members = vec!["Open".to_string(), "Closed".to_string()];
        assert!(value_matches_schema_type(
            &serde_json::json!("Open"),
            "Temper.Status",
            NamedTypeShape::ManifestEnum(&members),
        ));
        assert!(!value_matches_schema_type(
            &serde_json::json!("Other"),
            "Temper.Status",
            NamedTypeShape::ManifestEnum(&members),
        ));
        assert!(value_matches_schema_type(
            &serde_json::json!(["Open", "Closed"]),
            "Collection(Temper.Status)",
            NamedTypeShape::ManifestEnum(&members),
        ));
        assert!(!value_matches_schema_type(
            &serde_json::json!(["Open", "Other"]),
            "Collection(Temper.Status)",
            NamedTypeShape::ManifestEnum(&members),
        ));
        assert!(value_matches_schema_type(
            &serde_json::json!("12.50"),
            "Edm.Decimal",
            NamedTypeShape::Complex,
        ));
        assert!(!value_matches_schema_type(
            &serde_json::json!(12.5),
            "Edm.Decimal",
            NamedTypeShape::Complex,
        ));
    }
}
