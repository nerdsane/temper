use super::*;

#[test]
fn sequence_range_matches_redis_lua_exact_integer_contract() {
    assert_eq!(
        redis_sequence(MAX_SAFE_REDIS_SEQUENCE, "test").unwrap(),
        MAX_SAFE_REDIS_SEQUENCE as i64
    );
    assert!(redis_sequence(MAX_SAFE_REDIS_SEQUENCE + 1, "test").is_err());
    assert!(decoded_redis_sequence(-1, "test").is_err());
}
