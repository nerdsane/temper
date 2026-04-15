//! Shared naming convention utilities for converting between cases.

/// Convert a string to PascalCase.
///
/// "order" -> "Order", "order_item" -> "OrderItem", "my-entity" -> "MyEntity"
pub fn to_pascal_case(s: &str) -> String {
    s.split(['_', '-'])
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => {
                    let upper: String = first.to_uppercase().collect();
                    format!("{}{}", upper, chars.collect::<String>())
                }
                None => String::new(),
            }
        })
        .collect()
}

/// Convert a PascalCase or camelCase string to snake_case.
///
/// Handles consecutive uppercase gracefully (e.g. "HTMLParser" -> "html_parser").
///
/// "Order" -> "order", "OrderItem" -> "order_item"
pub fn to_snake_case(s: &str) -> String {
    let mut result = String::new();
    for (i, ch) in s.chars().enumerate() {
        if ch.is_uppercase() && i > 0 {
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
    fn test_to_pascal_case() {
        assert_eq!(to_pascal_case("order"), "Order");
        assert_eq!(to_pascal_case("order_item"), "OrderItem");
        assert_eq!(to_pascal_case("my-entity"), "MyEntity");
    }

    #[test]
    fn test_to_snake_case() {
        assert_eq!(to_snake_case("Order"), "order");
        assert_eq!(to_snake_case("OrderItem"), "order_item");
        assert_eq!(to_snake_case("Customer"), "customer");
    }
}
