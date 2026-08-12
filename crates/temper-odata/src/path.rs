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

    /// A keyed entity within a navigation collection,
    /// e.g. `/Orders('abc-123')/Items('item-1')`.
    NavigationEntity {
        parent: Box<ODataPath>,
        property: String,
        key: KeyValue,
    },

    /// A bound action on an entity, e.g. `/Orders('abc-123')/Namespace.CancelOrder`.
    BoundAction {
        parent: Box<ODataPath>,
        action: String,
    },

    /// A bound function on an entity or collection, e.g.
    /// `/Orders('abc-123')/Namespace.GetTotal()` or, with named arguments,
    /// `/Orders/Temper.Nearest(decl='taste',to='x',k=10)`. `params` holds the
    /// parsed `name=value` argument list in declared order (empty for `()`).
    BoundFunction {
        parent: Box<ODataPath>,
        function: String,
        params: Vec<(String, String)>,
    },

    /// The `$value` media stream of an entity, e.g. `/Files('f-1')/$value`.
    Value { parent: Box<ODataPath> },
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
    let path = path.strip_prefix('/').unwrap_or(path);

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
fn parse_continuation_segment(segment: &str, parent: ODataPath) -> Result<ODataPath, ODataError> {
    // $value: media stream endpoint
    if segment == "$value" {
        return Ok(ODataPath::Value {
            parent: Box::new(parent),
        });
    }

    // Check if this is a qualified name (contains dot) — bound operation
    if segment.contains('.') {
        // Bound function: a qualified name with a parenthesized argument list,
        // e.g. `Namespace.GetTotal()` or `Temper.Nearest(decl='taste',to='x',k=10)`.
        if let Some(paren_start) = segment.find('(') {
            if !segment.ends_with(')') {
                return Err(ODataError::InvalidPath {
                    message: format!("bound function '{segment}' has unmatched parenthesis"),
                });
            }
            // Keep the FULL qualified name (e.g. `Temper.Nearest`), unlike a bound
            // action (which resolves to a short spec action name). A kernel function
            // like `Temper.Nearest` is dispatched by its namespaced name, so a
            // same-named function in another namespace does not collide with it.
            let qualified_name = &segment[..paren_start];
            let args_str = &segment[paren_start + 1..segment.len() - 1];
            let params = parse_function_params(args_str)?;
            return Ok(ODataPath::BoundFunction {
                parent: Box::new(parent),
                function: qualified_name.to_string(),
                params,
            });
        }

        // Bound action: qualified name without parentheses.
        let action_name = segment.rsplit('.').next().unwrap_or(segment);
        return Ok(ODataPath::BoundAction {
            parent: Box::new(parent),
            action: action_name.to_string(),
        });
    }

    // Check for key access on a navigation property, e.g. Items(123).
    // This models `/Parent/Items(123)` as NavigationEntity, preserving the
    // parent context for hierarchical path resolution.
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

        return Ok(ODataPath::NavigationEntity {
            parent: Box::new(parent),
            property: name.to_string(),
            key,
        });
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
                // Unquote the value if present (collapses '' escapes).
                let value = parse_key_literal(&value)?;
                pairs.push((name, value));
            } else {
                return Err(ODataError::InvalidPath {
                    message: format!("invalid composite key part: '{part}'"),
                });
            }
        }
        Ok(KeyValue::Composite(pairs))
    } else {
        // Single key value — unquote if present (collapses '' escapes).
        let value = parse_key_literal(key_str)?;
        Ok(KeyValue::Single(value))
    }
}

/// Parse a bound-function argument list like `decl='taste',to='x',k=10` into an
/// ordered list of `(name, value)` pairs. Empty input yields an empty list (the
/// `Function()` case). Quoted values are unquoted (with `''` escapes collapsed) via
/// [`parse_key_literal`], and commas inside quotes do not split, so a raw-vector
/// argument such as `vector='[0.1, 0.2]'` parses as one value. Reuses the composite
/// key splitting/unquoting rules so URL argument syntax stays consistent.
fn parse_function_params(args_str: &str) -> Result<Vec<(String, String)>, ODataError> {
    if args_str.trim().is_empty() {
        return Ok(Vec::new());
    }
    let mut params: Vec<(String, String)> = Vec::new();
    for part in split_composite_key(args_str)? {
        let part = part.trim();
        let Some(eq_pos) = part.find('=') else {
            return Err(ODataError::InvalidPath {
                message: format!("bound function argument '{part}' is missing '='"),
            });
        };
        let name = part[..eq_pos].trim().to_string();
        if name.is_empty() {
            return Err(ODataError::InvalidPath {
                message: format!("bound function argument '{part}' has an empty name"),
            });
        }
        // Reject a repeated argument name rather than silently taking the first (or
        // last) — a caller who wrote `k=5,k=10` gets a clear error, not a guess.
        if params.iter().any(|(existing, _)| existing == &name) {
            return Err(ODataError::InvalidPath {
                message: format!("bound function argument '{name}' is given more than once"),
            });
        }
        let value = parse_key_literal(part[eq_pos + 1..].trim())?;
        params.push((name, value));
    }
    Ok(params)
}

/// String delimiters recognised around a key literal.
///
/// OData spells string keys with single quotes (`'abc'`). Clients that borrow
/// JSON habits emit double quotes (`"abc"`) instead, so both are recognised —
/// see [`parse_key_literal`] for why the delimiter must never survive parsing.
const KEY_DELIMITERS: [char; 2] = ['\'', '"'];

fn is_key_delimiter(ch: char) -> bool {
    KEY_DELIMITERS.contains(&ch)
}

/// Track whether the scanner is inside a quoted literal.
///
/// Only the delimiter that opened the literal closes it, so a double quote
/// inside `'it"s'` is ordinary text rather than a state change.
fn step_quote_state(open: Option<char>, ch: char) -> Option<char> {
    match open {
        Some(delim) if ch == delim => None,
        Some(delim) => Some(delim),
        None if is_key_delimiter(ch) => Some(ch),
        None => None,
    }
}

/// Check if a key expression is a composite key (has `=` outside quotes).
fn is_composite_key(s: &str) -> bool {
    let mut open: Option<char> = None;
    for ch in s.chars() {
        if ch == '=' && open.is_none() {
            return true;
        }
        open = step_quote_state(open, ch);
    }
    false
}

/// Split composite key parts by comma, respecting quotes.
fn split_composite_key(s: &str) -> Result<Vec<String>, ODataError> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut open: Option<char> = None;

    for ch in s.chars() {
        if ch == ',' && open.is_none() {
            parts.push(std::mem::take(&mut current));
            continue;
        }
        open = step_quote_state(open, ch);
        current.push(ch);
    }

    if open.is_some() {
        return Err(ODataError::InvalidPath {
            message: "unmatched quote in key value".into(),
        });
    }

    if !current.is_empty() {
        parts.push(current);
    }

    Ok(parts)
}

/// Parse an OData key literal, stripping the string delimiters.
///
/// Per OData v4 (ABNF `string` rule), a single quote inside a quoted literal
/// is escaped by doubling it: `'abc''123'` denotes the value `abc'123`. The
/// same rule is applied to double-quoted literals (`"abc"`), which OData does
/// not define but clients do send.
///
/// The delimiter must never survive into the returned value: the key becomes
/// the entity id, and an id that still carries its quotes fails Cedar's
/// `Type::"id"` UID parsing, turning an ordinary policy question into a 403
/// that no policy change can fix (ARN-287).
///
/// A bare (un-doubled) quote inside the literal, trailing text after the
/// closing quote, and an unterminated literal are all parse errors. Unquoted
/// literals (integers, GUIDs) are returned unchanged but must not contain
/// quote characters.
fn parse_key_literal(s: &str) -> Result<String, ODataError> {
    let Some(delimiter) = s.chars().next().filter(|ch| is_key_delimiter(*ch)) else {
        // Unquoted literal (integer, GUID, …) — quotes are not allowed.
        if s.contains(KEY_DELIMITERS) {
            return Err(ODataError::InvalidPath {
                message: format!("unquoted key literal '{s}' contains a quote"),
            });
        }
        return Ok(s.to_string());
    };

    let interior = &s[delimiter.len_utf8()..];
    let mut value = String::with_capacity(interior.len());
    let mut chars = interior.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != delimiter {
            value.push(ch);
            continue;
        }
        // Doubled delimiter — an escaped literal quote.
        if chars.peek() == Some(&delimiter) {
            chars.next();
            value.push(delimiter);
            continue;
        }
        // Lone delimiter — closes the literal; it must be the final character.
        if chars.next().is_some() {
            return Err(ODataError::InvalidPath {
                message: format!(
                    "invalid key literal {s}: a {delimiter} inside a key must be escaped by doubling it"
                ),
            });
        }
        return Ok(value);
    }

    Err(ODataError::InvalidPath {
        message: format!("unterminated quote in key literal {s}"),
    })
}

/// Validate that a string is a valid OData identifier.
fn validate_identifier(s: &str) -> Result<(), ODataError> {
    if s.is_empty() {
        return Err(ODataError::InvalidPath {
            message: "empty identifier".into(),
        });
    }

    // Must start with a letter or underscore
    let first = s.chars().next().unwrap(); // ci-ok: empty string checked above
    if !first.is_ascii_alphabetic() && first != '_' {
        return Err(ODataError::InvalidPath {
            message: format!("invalid identifier '{s}': must start with a letter or underscore"),
        });
    }

    // Remaining characters must be alphanumeric or underscore
    for ch in s.chars() {
        if !ch.is_ascii_alphanumeric() && ch != '_' {
            return Err(ODataError::InvalidPath {
                message: format!("invalid identifier '{s}': contains invalid character '{ch}'"),
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
        let result = parse_path("/Orders('abc-123')/Temper.Example.CancelOrder").unwrap();
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
        let result = parse_path("/Orders('abc-123')/Temper.Example.GetOrderTotal()").unwrap();
        assert_eq!(
            result,
            ODataPath::BoundFunction {
                parent: Box::new(ODataPath::Entity(
                    "Orders".into(),
                    KeyValue::Single("abc-123".into())
                )),
                function: "Temper.Example.GetOrderTotal".into(),
                params: vec![],
            }
        );
    }

    #[test]
    fn parse_collection_bound_function_with_named_params() {
        // ADR-0155: Temper.Nearest is a collection-bound function with named args.
        let result =
            parse_path("/DesignLanguages/Temper.Nearest(decl='taste',to='en-1',k=10)").unwrap();
        assert_eq!(
            result,
            ODataPath::BoundFunction {
                parent: Box::new(ODataPath::EntitySet("DesignLanguages".into())),
                function: "Temper.Nearest".into(),
                params: vec![
                    ("decl".into(), "taste".into()),
                    ("to".into(), "en-1".into()),
                    ("k".into(), "10".into()),
                ],
            }
        );
    }

    #[test]
    fn parse_bound_function_arg_value_may_contain_commas_in_quotes() {
        // A raw-vector argument is a quoted list; commas inside quotes do not split.
        let result = parse_path(
            "/DesignLanguages/Temper.Nearest(decl='taste',vector='[0.1, 0.2]',model='m')",
        )
        .unwrap();
        let ODataPath::BoundFunction { params, .. } = result else {
            panic!("expected bound function");
        };
        assert_eq!(params[1], ("vector".into(), "[0.1, 0.2]".into()));
        assert_eq!(params[2], ("model".into(), "m".into()));
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
    fn parse_navigation_entity_with_key() {
        let result = parse_path("/Orders('abc-123')/Items('item-1')").unwrap();
        assert_eq!(
            result,
            ODataPath::NavigationEntity {
                parent: Box::new(ODataPath::Entity(
                    "Orders".into(),
                    KeyValue::Single("abc-123".into())
                )),
                property: "Items".into(),
                key: KeyValue::Single("item-1".into()),
            }
        );
    }

    #[test]
    fn parse_navigation_entity_integer_key() {
        let result = parse_path("/Customers('c-1')/Orders(42)").unwrap();
        assert_eq!(
            result,
            ODataPath::NavigationEntity {
                parent: Box::new(ODataPath::Entity(
                    "Customers".into(),
                    KeyValue::Single("c-1".into())
                )),
                property: "Orders".into(),
                key: KeyValue::Single("42".into()),
            }
        );
    }

    #[test]
    fn parse_multi_level_navigation() {
        // /Orders('o-1')/Items('i-1')/Product
        let result = parse_path("/Orders('o-1')/Items('i-1')/Product").unwrap();
        assert_eq!(
            result,
            ODataPath::NavigationProperty {
                parent: Box::new(ODataPath::NavigationEntity {
                    parent: Box::new(ODataPath::Entity(
                        "Orders".into(),
                        KeyValue::Single("o-1".into())
                    )),
                    property: "Items".into(),
                    key: KeyValue::Single("i-1".into()),
                }),
                property: "Product".into(),
            }
        );
    }

    #[test]
    fn parse_navigation_then_bound_action() {
        // /Orders('o-1')/Items('i-1')/Temper.RemoveItem
        let result = parse_path("/Orders('o-1')/Items('i-1')/Temper.RemoveItem").unwrap();
        assert_eq!(
            result,
            ODataPath::BoundAction {
                parent: Box::new(ODataPath::NavigationEntity {
                    parent: Box::new(ODataPath::Entity(
                        "Orders".into(),
                        KeyValue::Single("o-1".into())
                    )),
                    property: "Items".into(),
                    key: KeyValue::Single("i-1".into()),
                }),
                action: "RemoveItem".into(),
            }
        );
    }

    #[test]
    fn parse_entity_key_with_escaped_quote() {
        // OData escapes a quote inside a key literal by doubling it.
        assert_eq!(
            parse_path("/Orders('abc''123')").unwrap(),
            ODataPath::Entity("Orders".into(), KeyValue::Single("abc'123".into()))
        );
    }

    #[test]
    fn parse_entity_key_only_escaped_quote() {
        // '''' is a quoted literal containing exactly one quote.
        assert_eq!(
            parse_path("/Orders('''')").unwrap(),
            ODataPath::Entity("Orders".into(), KeyValue::Single("'".into()))
        );
    }

    #[test]
    fn parse_entity_key_empty_quoted_string() {
        assert_eq!(
            parse_path("/Orders('')").unwrap(),
            ODataPath::Entity("Orders".into(), KeyValue::Single(String::new()))
        );
    }

    #[test]
    fn parse_entity_key_bare_interior_quote_rejected() {
        // A lone quote inside the literal must be escaped as ''.
        assert!(parse_path("/Orders('abc'123')").is_err());
    }

    #[test]
    fn parse_entity_key_unterminated_quote_rejected() {
        assert!(parse_path("/Orders('abc)").is_err());
    }

    #[test]
    fn parse_entity_key_unquoted_with_quote_rejected() {
        assert!(parse_path("/Orders(abc'def)").is_err());
    }

    #[test]
    fn parse_entity_key_double_quoted_matches_single_quoted() {
        // ARN-287: a double-quoted key must yield the same entity id as the
        // OData-spelled single-quoted one — the delimiter never reaches Cedar.
        assert_eq!(
            parse_path("/Files(\"fl-019efde0-3fa5\")").unwrap(),
            parse_path("/Files('fl-019efde0-3fa5')").unwrap()
        );
        assert_eq!(
            parse_path("/Files(\"fl-019efde0-3fa5\")").unwrap(),
            ODataPath::Entity("Files".into(), KeyValue::Single("fl-019efde0-3fa5".into()))
        );
    }

    #[test]
    fn parse_entity_key_double_quoted_with_escaped_quote() {
        assert_eq!(
            parse_path("/Orders(\"abc\"\"123\")").unwrap(),
            ODataPath::Entity("Orders".into(), KeyValue::Single("abc\"123".into()))
        );
    }

    #[test]
    fn parse_entity_key_single_quoted_keeps_interior_double_quotes() {
        // Only the outermost delimiter is stripped; a double quote inside a
        // single-quoted literal is ordinary key text.
        assert_eq!(
            parse_path("/Orders('a\"b')").unwrap(),
            ODataPath::Entity("Orders".into(), KeyValue::Single("a\"b".into()))
        );
    }

    #[test]
    fn parse_entity_key_unquoted_with_double_quote_rejected() {
        assert!(parse_path("/Orders(abc\"def)").is_err());
    }

    #[test]
    fn parse_entity_key_double_quoted_unterminated_rejected() {
        assert!(parse_path("/Orders(\"abc)").is_err());
    }

    #[test]
    fn parse_composite_key_double_quoted_values() {
        assert_eq!(
            parse_path("/OrderItems(OrderId=\"a,b\",LineNo=1)").unwrap(),
            ODataPath::Entity(
                "OrderItems".into(),
                KeyValue::Composite(vec![
                    ("OrderId".into(), "a,b".into()),
                    ("LineNo".into(), "1".into()),
                ])
            )
        );
    }

    #[test]
    fn parse_bound_function_double_quoted_argument() {
        assert_eq!(
            parse_path("/Refs/Temper.Nearest(decl=\"taste\",k=10)").unwrap(),
            ODataPath::BoundFunction {
                parent: Box::new(ODataPath::EntitySet("Refs".into())),
                function: "Temper.Nearest".into(),
                params: vec![("decl".into(), "taste".into()), ("k".into(), "10".into()),],
            }
        );
    }

    #[test]
    fn parse_composite_key_with_escaped_quotes() {
        let result = parse_path("/OrderItems(OrderId='a''b',LineNo='2''')").unwrap();
        assert_eq!(
            result,
            ODataPath::Entity(
                "OrderItems".into(),
                KeyValue::Composite(vec![
                    ("OrderId".into(), "a'b".into()),
                    ("LineNo".into(), "2'".into()),
                ])
            )
        );
    }

    #[test]
    fn parse_composite_key_bare_quote_rejected() {
        assert!(parse_path("/OrderItems(OrderId='a'b',LineNo=1)").is_err());
    }

    #[test]
    fn parse_navigation_entity_key_with_escaped_quote() {
        let result = parse_path("/Orders('o''1')/Items('i''2')").unwrap();
        assert_eq!(
            result,
            ODataPath::NavigationEntity {
                parent: Box::new(ODataPath::Entity(
                    "Orders".into(),
                    KeyValue::Single("o'1".into())
                )),
                property: "Items".into(),
                key: KeyValue::Single("i'2".into()),
            }
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

    #[test]
    fn parse_value_on_entity() {
        let result = parse_path("/Files('f-1')/$value").unwrap();
        assert_eq!(
            result,
            ODataPath::Value {
                parent: Box::new(ODataPath::Entity(
                    "Files".into(),
                    KeyValue::Single("f-1".into())
                )),
            }
        );
    }

    #[test]
    fn parse_value_on_navigation_entity() {
        let result = parse_path("/Directories('d-1')/Files('f-1')/$value").unwrap();
        assert_eq!(
            result,
            ODataPath::Value {
                parent: Box::new(ODataPath::NavigationEntity {
                    parent: Box::new(ODataPath::Entity(
                        "Directories".into(),
                        KeyValue::Single("d-1".into())
                    )),
                    property: "Files".into(),
                    key: KeyValue::Single("f-1".into()),
                }),
            }
        );
    }

    #[test]
    fn parse_bare_value_fails() {
        // $value at root level is not valid
        let result = parse_path("/$value");
        assert!(result.is_err());
    }
}
