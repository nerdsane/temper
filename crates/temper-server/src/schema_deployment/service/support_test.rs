use super::*;

#[test]
fn canonical_json_sorts_nested_object_keys() {
    let left = serde_json::json!({"z": {"b": 2, "a": 1}, "a": [{"d": 4, "c": 3}]});
    let right = serde_json::json!({"a": [{"c": 3, "d": 4}], "z": {"a": 1, "b": 2}});
    let left = canonical_json_object(&left).unwrap();
    let right = canonical_json_object(&right).unwrap();
    assert_eq!(left, right);
    assert_eq!(left, r#"{"a":[{"c":3,"d":4}],"z":{"a":1,"b":2}}"#);
}
