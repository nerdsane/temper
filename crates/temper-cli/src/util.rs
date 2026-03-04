/// Convert a string to PascalCase.
///
/// "order" -> "Order", "order_item" -> "OrderItem"
pub(crate) fn to_pascal_case(s: &str) -> String {
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
/// "Order" -> "order", "OrderItem" -> "order_item"
pub(crate) fn to_snake_case(s: &str) -> String {
    let mut result = String::new();
    for (i, c) in s.chars().enumerate() {
        if c.is_uppercase() {
            if i > 0 {
                result.push('_');
            }
            result.extend(c.to_lowercase());
        } else {
            result.push(c);
        }
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
