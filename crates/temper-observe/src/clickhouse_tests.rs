use super::*;

#[test]
fn test_store_construction() {
    let store = ClickHouseStore::new("http://localhost:8123");
    assert_eq!(store.base_url(), "http://localhost:8123");
}

#[test]
fn bind_params_uses_clickhouse_typed_placeholders() {
    let sql = "SELECT * FROM spans WHERE service = $1 AND duration_ns > $2";
    let params = vec![SqlParam::String("api".into()), SqlParam::Int(1000)];
    let result = ClickHouseStore::bind_params(sql, &params).unwrap();
    assert_eq!(
        result,
        BoundClickHouseQuery {
            sql: "SELECT * FROM spans WHERE service = {p1:String} AND duration_ns > {p2:Int64}"
                .into(),
            form_params: vec![
                ("param_p1".into(), "api".into()),
                ("param_p2".into(), "1000".into()),
            ],
        }
    );
}

#[test]
fn bind_params_preserves_placeholders_in_quoted_regions_and_comments() {
    let sql = "SELECT '$1', \"$1\", `$1`, $1 -- $1\n/* $1 */ # $1\n";
    let result = ClickHouseStore::bind_params(sql, &[SqlParam::String("api".into())]).unwrap();
    assert_eq!(
        result.sql,
        "SELECT '$1', \"$1\", `$1`, {p1:String} -- $1\n/* $1 */ # $1\n"
    );
}

#[test]
fn bind_params_preserves_clickhouse_extended_lexical_regions() {
    let sql = "SELECT $1 // $2\n/* outer $2 /* nested */ still $2 */\n$$$$, $$body $2$$, $tag$body $2$tag$, ‘$2’, “$2”, $2";
    let result =
        ClickHouseStore::bind_params(sql, &[SqlParam::String("first".into()), SqlParam::Int(2)])
            .unwrap();
    assert_eq!(
        result.sql,
        "SELECT {p1:String} // $2\n/* outer $2 /* nested */ still $2 */\n$$$$, $$body $2$$, $tag$body $2$tag$, ‘$2’, “$2”, {p2:Int64}"
    );
}

#[test]
fn bind_params_handles_repetition_and_escaped_quotes() {
    let sql = r#"SELECT '$1''$1', 'escaped \' $1', "$1""$1", `$1``$1`, metric$1, 1$1, $1, $1"#;
    let result = ClickHouseStore::bind_params(sql, &[SqlParam::Int(7)]).unwrap();
    assert_eq!(
        result.sql,
        r#"SELECT '$1''$1', 'escaped \' $1', "$1""$1", `$1``$1`, metric$1, 1$1, {p1:Int64}, {p1:Int64}"#
    );
    assert_eq!(result.form_params, vec![("param_p1".into(), "7".into())]);
}

#[test]
fn bind_params_never_places_attacker_text_in_sql() {
    let attack = "x\\' OR 1=1; DROP TABLE spans; -- $2";
    let result = ClickHouseStore::bind_params(
        "SELECT * FROM spans WHERE a = $1 AND b = $2",
        &[
            SqlParam::String(attack.into()),
            SqlParam::String("second".into()),
        ],
    )
    .unwrap();

    assert_eq!(
        result.sql,
        "SELECT * FROM spans WHERE a = {p1:String} AND b = {p2:String}"
    );
    assert!(!result.sql.contains("DROP TABLE"));
    assert_eq!(result.form_params[0].1, attack);
}

#[test]
fn bind_params_handles_types_two_digits_and_multibyte_text() {
    let sql = "SELECT $10, name = $1, note = 'café ☕'";
    let mut params: Vec<SqlParam> = (1..=10).map(SqlParam::Int).collect();
    params[0] = SqlParam::String("first".into());
    let result = ClickHouseStore::bind_params(sql, &params).unwrap();
    assert_eq!(
        result.sql,
        "SELECT {p10:Int64}, name = {p1:String}, note = 'café ☕'"
    );
    assert_eq!(
        result.form_params,
        vec![
            ("param_p1".into(), "first".into()),
            ("param_p10".into(), "10".into()),
        ]
    );
}

#[test]
fn bind_params_handles_null_bool_and_float() {
    let result = ClickHouseStore::bind_params(
        "SELECT $1, $2, $3",
        &[SqlParam::Null, SqlParam::Bool(true), SqlParam::Float(1.5)],
    )
    .unwrap();
    assert_eq!(result.sql, "SELECT NULL, {p2:UInt8}, {p3:Float64}");
    assert_eq!(
        result.form_params,
        vec![
            ("param_p2".into(), "1".into()),
            ("param_p3".into(), "1.5".into()),
        ]
    );
}

#[test]
fn bind_params_rejects_missing_and_zero_parameters() {
    let missing = ClickHouseStore::bind_params("SELECT $2", &[SqlParam::Int(1)]);
    assert!(matches!(missing, Err(ObserveError::InvalidQuery(_))));

    let zero = ClickHouseStore::bind_params("SELECT $0", &[SqlParam::Int(1)]);
    assert!(matches!(zero, Err(ObserveError::InvalidQuery(_))));
}

#[test]
fn test_parse_empty() {
    let rs = parse_json_each_row("").unwrap();
    assert!(rs.is_empty());
}

#[test]
fn test_parse_single_row() {
    let body = r#"{"service":"api","status":"ok"}"#;
    let rs = parse_json_each_row(body).unwrap();
    assert_eq!(rs.len(), 1);
    assert_eq!(rs.get(0, "service"), Some(&serde_json::json!("api")));
}

#[test]
fn test_parse_multiple_rows() {
    let body = "{\"a\":1}\n{\"a\":2}";
    let rs = parse_json_each_row(body).unwrap();
    assert_eq!(rs.len(), 2);
}

#[test]
fn build_request_keeps_parameters_out_of_url() {
    let attack = "x\\' OR 1=1 --";
    let store = ClickHouseStore::new("http://127.0.0.1:8123");
    let request = store
        .build_request(
            "SELECT service FROM spans WHERE service = $1",
            &[SqlParam::String(attack.into())],
        )
        .unwrap();

    assert_eq!(request.method(), reqwest::Method::POST);
    assert_eq!(request.url().path(), "/");
    assert_eq!(request.url().query(), Some("default_format=JSONEachRow"));
    assert!(!request.url().as_str().contains(attack));
    assert!(
        request
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("multipart/form-data; boundary="))
    );
}
