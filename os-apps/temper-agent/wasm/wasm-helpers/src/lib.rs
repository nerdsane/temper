//! Shared helper functions for TemperAgent WASM modules.
//!
//! Provides common TemperFS I/O, field extraction, and URL resolution
//! to eliminate duplication across WASM integration modules.

use temper_wasm_sdk::prelude::*;

/// Resolve the Temper API URL from entity fields or context config,
/// falling back to localhost.
pub fn resolve_temper_api_url(ctx: &Context, fields: &Value) -> String {
    fields
        .get("temper_api_url")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .or_else(|| {
            ctx.config
                .get("temper_api_url")
                .filter(|s| !s.is_empty())
                .cloned()
        })
        .unwrap_or_else(|| "http://127.0.0.1:3000".to_string())
}

/// Read session JSONL from TemperFS by file ID.
pub fn read_session_from_temperfs(
    ctx: &Context,
    temper_api_url: &str,
    tenant: &str,
    file_id: &str,
) -> Result<String, String> {
    let url = format!("{temper_api_url}/tdata/Files('{file_id}')/$value");
    let headers = vec![
        ("x-tenant-id".to_string(), tenant.to_string()),
        ("x-temper-principal-kind".to_string(), "admin".to_string()),
    ];

    let resp = ctx.http_call("GET", &url, &headers, "")?;
    if resp.status == 200 {
        Ok(resp.body)
    } else if resp.status == 404 {
        Ok(String::new())
    } else {
        Err(format!("TemperFS session read failed (HTTP {})", resp.status))
    }
}

/// Write session JSONL to TemperFS by file ID.
pub fn write_session_to_temperfs(
    ctx: &Context,
    temper_api_url: &str,
    tenant: &str,
    file_id: &str,
    jsonl: &str,
) -> Result<(), String> {
    let url = format!("{temper_api_url}/tdata/Files('{file_id}')/$value");
    let headers = vec![
        ("content-type".to_string(), "text/plain".to_string()),
        ("x-tenant-id".to_string(), tenant.to_string()),
        ("x-temper-principal-kind".to_string(), "admin".to_string()),
    ];

    let resp = ctx.http_call("PUT", &url, &headers, jsonl)?;
    if resp.status >= 200 && resp.status < 300 {
        Ok(())
    } else {
        Err(format!("TemperFS session write failed (HTTP {})", resp.status))
    }
}

/// Build standard OData headers for tenant-scoped requests.
pub fn odata_headers(tenant: &str) -> Vec<(String, String)> {
    vec![
        ("x-tenant-id".to_string(), tenant.to_string()),
        ("x-temper-principal-kind".to_string(), "admin".to_string()),
        ("content-type".to_string(), "application/json".to_string()),
        ("accept".to_string(), "application/json".to_string()),
    ]
}

/// Look up a string field directly on a JSON value, trying multiple key names.
pub fn direct_field_str<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_str))
}

/// Look up a string field on a JSON value, falling back to nested `fields` object.
pub fn entity_field_str<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a str> {
    direct_field_str(value, keys).or_else(|| {
        value
            .get("fields")
            .and_then(|fields| direct_field_str(fields, keys))
    })
}

/// Parse a basic ISO 8601 timestamp (YYYY-MM-DDTHH:MM:SSZ) to Unix epoch seconds.
/// Returns None if the format is unrecognized.
pub fn parse_iso8601_to_epoch_secs(s: &str) -> Option<u64> {
    // Supported formats: "2026-03-24T12:30:00Z", "2026-03-24T12:30:00.000Z"
    let s = s.trim();
    if s.len() < 19 {
        return None;
    }

    let year: u64 = s.get(0..4)?.parse().ok()?;
    let month: u64 = s.get(5..7)?.parse().ok()?;
    let day: u64 = s.get(8..10)?.parse().ok()?;
    let hour: u64 = s.get(11..13)?.parse().ok()?;
    let minute: u64 = s.get(14..16)?.parse().ok()?;
    let second: u64 = s.get(17..19)?.parse().ok()?;

    if s.as_bytes().get(4) != Some(&b'-')
        || s.as_bytes().get(7) != Some(&b'-')
        || s.as_bytes().get(10) != Some(&b'T')
    {
        return None;
    }

    // Days in each month (non-leap)
    let days_in_month = [0, 31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let is_leap = (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0);

    // Days from epoch (1970-01-01) to start of `year`
    let mut days: u64 = 0;
    for y in 1970..year {
        let leap = (y % 4 == 0 && y % 100 != 0) || (y % 400 == 0);
        days += if leap { 366 } else { 365 };
    }

    // Days from start of year to start of month
    for m in 1..month {
        days += days_in_month[m as usize];
        if m == 2 && is_leap {
            days += 1;
        }
    }

    // Days within month (1-indexed)
    days += day - 1;

    Some(days * 86400 + hour * 3600 + minute * 60 + second)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_iso8601() {
        // 2026-03-24T12:00:00Z
        let secs = parse_iso8601_to_epoch_secs("2026-03-24T12:00:00Z");
        assert!(secs.is_some());
        let s = secs.unwrap();
        // Rough sanity: should be > 2025-01-01 (~1735689600) and < 2027-01-01
        assert!(s > 1_735_000_000);
        assert!(s < 1_800_000_000);
    }

    #[test]
    fn test_parse_iso8601_with_millis() {
        let secs = parse_iso8601_to_epoch_secs("2026-03-24T12:00:00.123Z");
        assert!(secs.is_some());
    }

    #[test]
    fn test_parse_iso8601_invalid() {
        assert!(parse_iso8601_to_epoch_secs("").is_none());
        assert!(parse_iso8601_to_epoch_secs("not-a-date").is_none());
        assert!(parse_iso8601_to_epoch_secs("2026").is_none());
    }

    #[test]
    fn test_epoch_zero() {
        let secs = parse_iso8601_to_epoch_secs("1970-01-01T00:00:00Z");
        assert_eq!(secs, Some(0));
    }

    #[test]
    fn test_direct_field_str() {
        let val = serde_json::json!({"Name": "test", "id": "123"});
        assert_eq!(direct_field_str(&val, &["Name"]), Some("test"));
        assert_eq!(direct_field_str(&val, &["missing", "id"]), Some("123"));
        assert_eq!(direct_field_str(&val, &["missing"]), None);
    }

    #[test]
    fn test_entity_field_str() {
        let val = serde_json::json!({"fields": {"Status": "Active"}});
        assert_eq!(entity_field_str(&val, &["Status"]), Some("Active"));
    }
}
