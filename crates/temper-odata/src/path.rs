//! OData v4 URL path parser.
//!
//! Parses OData URL paths into a structured [`ODataPath`] enum.
//!
//! Supported paths:
//! - `/` or empty → ServiceDocument
//! - `/$metadata` → Metadata
//! - `/EntitySet` → EntitySet
//! - `/EntitySet('key')` → Entity with key
//! - `/EntitySet('key')/NavProperty` → NavigationProperty
//! - `/EntitySet('key')/Namespace.Action` → BoundAction
//! - `/EntitySet('key')/Namespace.Function()` → BoundFunction

use crate::error::ODataError;

/// Represents a parsed OData v4 URL path.
#[derive(Debug, Clone, PartialEq)]
pub enum ODataPath {
    /// The OData service document root (`/`).
    ServiceDocument,

    /// The `$metadata` document (`/$metadata`).
    Metadata,

    /// An entity set, e.g. `/Orders`.
    EntitySet(String),

    /// A single entity addressed by key, e.g. `/Orders('abc-123')`.
    Entity(String, KeyValue),

    /// A navigation property from an entity, e.g. `/Orders('abc-123')/Items`.
    NavigationProperty {
        parent: Box<ODataPath>,
        property: String,
    },

    /// A bound action on an entity, e.g. `/Orders('abc-123')/Namespace.CancelOrder`.
    BoundAction {
        parent: Box<ODataPath>,
        action: String,
    },

    /// A bound function on an entity, e.g. `/Orders('abc-123')/Namespace.GetTotal()`.
    BoundFunction {
        parent: Box<ODataPath>,
        function: String,
    },
}

/// Represents an entity key value.
#[derive(Debug, Clone, PartialEq)]
pub enum KeyValue {
    /// A single key value (string, integer, or GUID).
    Single(String),
    /// A composite key with named properties, e.g. `(OrderId='abc',LineNo=1)`.
    Composite(Vec<(String, String)>),
}

/// Parse an OData URL path string into an [`ODataPath`].
///
/// The path should be the URL path component (before the `?` query string).
pub fn parse_path(path: &str) -> Result<ODataPath, ODataError> {
    let path = path.trim();

    // Normalize: strip trailing slash (unless it's just "/")
    let path = if path.len() > 1 && path.ends_with('/') {
        &path[..path.len() - 1]
    } else {
        path
    };

    // Service document
    if path.is_empty() || path == "/" {
        return Ok(ODataPath::ServiceDocument);
    }

    // Must start with /
    let path = if path.starts_with('/') {
        &path[1..]
    } else {
        path
    };

    // $metadata
    if path == "$metadata" {
        return Ok(ODataPath::Metadata);
    }

    // Split into segments by '/' but we need to handle parentheses carefully.
    let segments = split_path_segments(path)?;

    if segments.is_empty() {
        return Ok(ODataPath::ServiceDocument);
    }

    // Parse the first segment — must be an entity set, possibly with a key.
    let first = &segments[0];
    let base = parse_entity_segment(first)?;

    // Process remaining segments by folding them onto the base.
    let mut current = base;
    for segment in &segments[1..] {
        current = parse_continuation_segment(segment, current)?;
    }

    Ok(current)
}

/// Split a path (without leading /) into segments, respecting parentheses.
fn split_path_segments(path: &str) -> Result<Vec<String>, ODataError> {
    let mut segments = Vec::new();
    let mut current = String::new();
    let mut paren_depth: u32 = 0;

    for ch in path.chars() {
        match ch {
            '(' => {
                paren_depth += 1;
                current.push(ch);
            }
            ')' => {
                if paren_depth == 0 {
                    return Err(ODataError::InvalidPath {
                        message: "unmatched closing parenthesis".into(),
                    });
                }
                paren_depth -= 1;
                current.push(ch);
            }
            '/' if paren_depth == 0 => {
                if !current.is_empty() {
                    segments.push(std::mem::take(&mut current));
                }
            }
            _ => {
                current.push(ch);
            }
        }
    }

    if paren_depth != 0 {
        return Err(ODataError::InvalidPath {
            message: "unmatched opening parenthesis".into(),
        });
    }

    if !current.is_empty() {
        segments.push(current);
    }

    Ok(segments)
}

/// Parse a segment like `Orders` or `Orders('abc-123')` into an ODataPath.
fn parse_entity_segment(segment: &str) -> Result<ODataPath, ODataError> {
    if let Some(paren_start) = segment.find('(') {
        let name = &segment[..paren_start];
        validate_identifier(name)?;

        // Extract the key expression between ( and )
        if !segment.ends_with(')') {
            return Err(ODataError::InvalidPath {
                message: format!("segment '{segment}' has unmatched parenthesis"),
            });
        }
        let key_str = &segment[paren_start + 1..segment.len() - 1];
        let key = parse_key_value(key_str)?;

        Ok(ODataPath::Entity(name.to_string(), key))
    } else {
        validate_identifier(segment)?;
        Ok(ODataPath::EntitySet(segment.to_string()))
    }
}

/// Parse a continuation segment after the first entity set/entity segment.
///
/// This could be:
/// - A simple navigation property: `Items`
/// - A qualified bound action: `Namespace.Action` (contains a dot, no parentheses at end)
/// - A qualified bound function: `Namespace.Function()` (contains a dot, ends with `()`)
/// - Another entity set access with key: `Items(123)`
fn parse_continuation_segment(
    segment: &str,
    parent: ODataPath,
) -> Result<ODataPath, ODataError> {
    // Check if this is a qualified name (contains dot) — bound operation
    if segment.contains('.') {
        // Bound function: ends with ()
        if segment.ends_with("()") {
            let qualified_name = &segment[..segment.len() - 2];
            // Extract just the operation name (last part after final dot)
            let function_name = qualified_name
                .rsplit('.')
                .next()
                .unwrap_or(qualified_name);
            return Ok(ODataPath::BoundFunction {
                parent: Box::new(parent),
                function: function_name.to_string(),
            });
        }

        // Bound action: qualified name without trailing ()
        let action_name = segment.rsplit('.').next().unwrap_or(segment);
        return Ok(ODataPath::BoundAction {
            parent: Box::new(parent),
            action: action_name.to_string(),
        });
    }

    // Check for key access on a navigation property, e.g. Items(123).
    // This models `/Parent/Items(123)` as Entity inside a NavigationProperty.
    if let Some(paren_start) = segment.find('(') {
        let name = &segment[..paren_start];
        validate_identifier(name)?;

        if !segment.ends_with(')') {
            return Err(ODataError::InvalidPath {
                message: format!("segment '{segment}' has unmatched parenthesis"),
            });
        }
        let key_str = &segment[paren_start + 1..segment.len() - 1];
        let key = parse_key_value(key_str)?;

        // Model as entity access within the parent's navigation collection.
        return Ok(ODataPath::Entity(name.to_string(), key));
    }

    // Simple navigation property
    validate_identifier(segment)?;
    Ok(ODataPath::NavigationProperty {
        parent: Box::new(parent),
        property: segment.to_string(),
    })
}

/// Parse a key value expression like `'abc-123'` or `OrderId='abc',LineNo=1`.
fn parse_key_value(key_str: &str) -> Result<KeyValue, ODataError> {
    let key_str = key_str.trim();

    if key_str.is_empty() {
        return Err(ODataError::InvalidPath {
            message: "empty key value".into(),
        });
    }

    // Check for composite key: contains '=' outside of quotes
    if is_composite_key(key_str) {
        let parts = split_composite_key(key_str)?;
        let mut pairs = Vec::new();
        for part in parts {
            let part = part.trim();
            if let Some(eq_pos) = part.find('=') {
                let name = part[..eq_pos].trim().to_string();
                let value = part[eq_pos + 1..].trim().to_string();
                // Strip quotes from value if present
                let value = strip_quotes(&value);
                pairs.push((name, value));
            } else {
                return Err(ODataError::InvalidPath {
                    message: format!("invalid composite key part: '{part}'"),
                });
            }
        }
        Ok(KeyValue::Composite(pairs))
    } else {
        // Single key value — strip surrounding quotes if present
        let value = strip_quotes(key_str);
        Ok(KeyValue::Single(value))
    }
}

/// Check if a key expression is a composite key (has `=` outside quotes).
fn is_composite_key(s: &str) -> bool {
    let mut in_quotes = false;
    for ch in s.chars() {
        match ch {
            '\'' => in_quotes = !in_quotes,
            '=' if !in_quotes => return true,
            _ => {}
        }
    }
    false
}

/// Split composite key parts by comma, respecting quotes.
fn split_composite_key(s: &str) -> Result<Vec<String>, ODataError> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;

    for ch in s.chars() {
        match ch {
            '\'' => {
                in_quotes = !in_quotes;
                current.push(ch);
            }
            ',' if !in_quotes => {
                parts.push(std::mem::take(&mut current));
            }
            _ => {
                current.push(ch);
            }
        }
    }

    if in_quotes {
        return Err(ODataError::InvalidPath {
            message: "unmatched quote in key value".into(),
        });
    }

    if !current.is_empty() {
        parts.push(current);
    }

    Ok(parts)
}

/// Strip surrounding single quotes from a string.
fn strip_quotes(s: &str) -> String {
    if s.len() >= 2 && s.starts_with('\'') && s.ends_with('\'') {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

/// Validate that a string is a valid OData identifier.
fn validate_identifier(s: &str) -> Result<(), ODataError> {
    if s.is_empty() {
        return Err(ODataError::InvalidPath {
            message: "empty identifier".into(),
        });
    }

    // Must start with a letter or underscore
    let first = s.chars().next().unwrap();
    if !first.is_ascii_alphabetic() && first != '_' {
        return Err(ODataError::InvalidPath {
            message: format!("invalid identifier '{s}': must start with a letter or underscore"),
        });
    }

    // Remaining characters must be alphanumeric or underscore
    for ch in s.chars() {
        if !ch.is_ascii_alphanumeric() && ch != '_' {
            return Err(ODataError::InvalidPath {
                message: format!(
                    "invalid identifier '{s}': contains invalid character '{ch}'"
                ),
            });
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_service_document_root() {
        assert_eq!(parse_path("/").unwrap(), ODataPath::ServiceDocument);
    }

    #[test]
    fn parse_service_document_empty() {
        assert_eq!(parse_path("").unwrap(), ODataPath::ServiceDocument);
    }

    #[test]
    fn parse_metadata() {
        assert_eq!(parse_path("/$metadata").unwrap(), ODataPath::Metadata);
    }

    #[test]
    fn parse_entity_set() {
        assert_eq!(
            parse_path("/Orders").unwrap(),
            ODataPath::EntitySet("Orders".into())
        );
    }

    #[test]
    fn parse_entity_with_string_key() {
        assert_eq!(
            parse_path("/Orders('abc-123')").unwrap(),
            ODataPath::Entity("Orders".into(), KeyValue::Single("abc-123".into()))
        );
    }

    #[test]
    fn parse_entity_with_integer_key() {
        assert_eq!(
            parse_path("/Products(42)").unwrap(),
            ODataPath::Entity("Products".into(), KeyValue::Single("42".into()))
        );
    }

    #[test]
    fn parse_navigation_property() {
        let result = parse_path("/Orders('abc-123')/Items").unwrap();
        assert_eq!(
            result,
            ODataPath::NavigationProperty {
                parent: Box::new(ODataPath::Entity(
                    "Orders".into(),
                    KeyValue::Single("abc-123".into())
                )),
                property: "Items".into(),
            }
        );
    }

    #[test]
    fn parse_bound_action() {
        let result = parse_path("/Orders('abc-123')/Temper.Ecommerce.CancelOrder").unwrap();
        assert_eq!(
            result,
            ODataPath::BoundAction {
                parent: Box::new(ODataPath::Entity(
                    "Orders".into(),
                    KeyValue::Single("abc-123".into())
                )),
                action: "CancelOrder".into(),
            }
        );
    }

    #[test]
    fn parse_bound_function() {
        let result =
            parse_path("/Orders('abc-123')/Temper.Ecommerce.GetOrderTotal()").unwrap();
        assert_eq!(
            result,
            ODataPath::BoundFunction {
                parent: Box::new(ODataPath::Entity(
                    "Orders".into(),
                    KeyValue::Single("abc-123".into())
                )),
                function: "GetOrderTotal".into(),
            }
        );
    }

    #[test]
    fn parse_composite_key() {
        let result = parse_path("/OrderItems(OrderId='abc',LineNo=1)").unwrap();
        assert_eq!(
            result,
            ODataPath::Entity(
                "OrderItems".into(),
                KeyValue::Composite(vec![
                    ("OrderId".into(), "abc".into()),
                    ("LineNo".into(), "1".into()),
                ])
            )
        );
    }

    #[test]
    fn parse_trailing_slash_is_stripped() {
        assert_eq!(
            parse_path("/Orders/").unwrap(),
            ODataPath::EntitySet("Orders".into())
        );
    }

    #[test]
    fn parse_invalid_empty_parens_key() {
        let result = parse_path("/Orders()");
        assert!(result.is_err());
    }

    #[test]
    fn parse_unmatched_paren() {
        let result = parse_path("/Orders('abc-123'");
        assert!(result.is_err());
    }
}
