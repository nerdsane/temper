// WASM integration module: fetches weather data from the Open-Meteo API
// and returns it as a FetchSucceeded callback with weather parameters.

extern "C" {
    fn host_get_context(buf_ptr: i32, buf_len: i32) -> i32;
    fn host_set_result(ptr: i32, len: i32);
    fn host_http_call(
        method_ptr: i32,
        method_len: i32,
        url_ptr: i32,
        url_len: i32,
        headers_ptr: i32,
        headers_len: i32,
        body_ptr: i32,
        body_len: i32,
        resp_ptr: i32,
        resp_len: i32,
    ) -> i32;
    fn host_log(level_ptr: i32, level_len: i32, msg_ptr: i32, msg_len: i32);
}

fn log(level: &str, msg: &str) {
    unsafe {
        host_log(
            level.as_ptr() as i32,
            level.len() as i32,
            msg.as_ptr() as i32,
            msg.len() as i32,
        );
    }
}

fn set_result(json: &str) {
    unsafe {
        host_set_result(json.as_ptr() as i32, json.len() as i32);
    }
}

/// Extract a JSON string value by key from a flat JSON object.
/// Minimal parser — no dependencies needed for wasm32-unknown-unknown.
fn extract_json_string<'a>(json: &'a str, key: &str) -> Option<&'a str> {
    let search = format!("\"{}\":\"", key);
    let alt_search = format!("\"{}\": \"", key);

    let start = json
        .find(&search)
        .map(|i| i + search.len())
        .or_else(|| json.find(&alt_search).map(|i| i + alt_search.len()))?;

    let rest = &json[start..];
    let end = rest.find('"')?;
    Some(&rest[..end])
}

/// Extract a JSON number value by key (returned as string slice).
fn extract_json_number<'a>(json: &'a str, key: &str) -> Option<&'a str> {
    let search = format!("\"{}\":", key);
    let start = json.find(&search).map(|i| i + search.len())?;
    let rest = json[start..].trim_start();
    let end = rest
        .find(|c: char| c != '-' && c != '.' && !c.is_ascii_digit())
        .unwrap_or(rest.len());
    if end == 0 {
        return None;
    }
    Some(&rest[..end])
}

/// Map WMO weather code to human-readable condition string.
fn wmo_code_to_condition(code: i32) -> &'static str {
    match code {
        0 => "Clear sky",
        1 => "Mainly clear",
        2 => "Partly cloudy",
        3 => "Overcast",
        45 | 48 => "Foggy",
        51 | 53 | 55 => "Drizzle",
        56 | 57 => "Freezing drizzle",
        61 | 63 | 65 => "Rain",
        66 | 67 => "Freezing rain",
        71 | 73 | 75 => "Snow",
        77 => "Snow grains",
        80 | 81 | 82 => "Rain showers",
        85 | 86 => "Snow showers",
        95 => "Thunderstorm",
        96 | 99 => "Thunderstorm with hail",
        _ => "Unknown",
    }
}

fn fail(reason: &str) {
    log("error", reason);
    let result = format!(
        r#"{{"action":"FetchFailed","params":{{"reason":"{}"}},"success":false}}"#,
        reason
    );
    set_result(&result);
}

#[unsafe(no_mangle)]
pub extern "C" fn run(_ctx_ptr: i32, _ctx_len: i32) -> i32 {
    // 1. Read invocation context
    let mut ctx_buf = [0u8; 4096];
    let ctx_len =
        unsafe { host_get_context(ctx_buf.as_mut_ptr() as i32, ctx_buf.len() as i32) };
    if ctx_len <= 0 {
        fail("Failed to read invocation context");
        return 0;
    }
    let ctx_str = match core::str::from_utf8(&ctx_buf[..ctx_len as usize]) {
        Ok(s) => s,
        Err(_) => {
            fail("Invalid UTF-8 in context");
            return 0;
        }
    };

    log("info", "fetch_open_meteo: reading entity state for coordinates");

    // 2. Extract lat/lon from entity_state in context
    let lat = extract_json_string(ctx_str, "Latitude")
        .or_else(|| extract_json_number(ctx_str, "Latitude"));
    let lon = extract_json_string(ctx_str, "Longitude")
        .or_else(|| extract_json_number(ctx_str, "Longitude"));

    let (lat, lon) = match (lat, lon) {
        (Some(la), Some(lo)) => (la, lo),
        _ => {
            fail("Missing Latitude/Longitude in entity state");
            return 0;
        }
    };

    log("info", &format!("fetch_open_meteo: fetching for lat={}, lon={}", lat, lon));

    // 3. Build Open-Meteo API URL
    let url = format!(
        "https://api.open-meteo.com/v1/forecast?latitude={}&longitude={}&current=temperature_2m,relative_humidity_2m,wind_speed_10m,precipitation,weather_code",
        lat, lon
    );
    let method = b"GET";
    let headers = b"";
    let body = b"";
    let mut resp_buf = [0u8; 8192];

    let bytes_written = unsafe {
        host_http_call(
            method.as_ptr() as i32,
            method.len() as i32,
            url.as_ptr() as i32,
            url.len() as i32,
            headers.as_ptr() as i32,
            headers.len() as i32,
            body.as_ptr() as i32,
            body.len() as i32,
            resp_buf.as_mut_ptr() as i32,
            resp_buf.len() as i32,
        )
    };

    if bytes_written < 0 {
        fail("HTTP call to Open-Meteo failed");
        return 0;
    }

    let resp_str = match core::str::from_utf8(&resp_buf[..bytes_written as usize]) {
        Ok(s) => s,
        Err(_) => {
            fail("Invalid UTF-8 in Open-Meteo response");
            return 0;
        }
    };

    // Response format from host: "status\nbody"
    let newline_pos = match resp_str.find('\n') {
        Some(p) => p,
        None => {
            fail("Malformed HTTP response (no status line)");
            return 0;
        }
    };

    let status = &resp_str[..newline_pos];
    let body = &resp_str[newline_pos + 1..];

    if !status.starts_with("200") {
        fail(&format!("Open-Meteo returned status {}", status));
        return 0;
    }

    log("info", "fetch_open_meteo: parsing Open-Meteo response");

    // 4. Parse the "current" block from Open-Meteo response
    let current_start = match body.find("\"current\"") {
        Some(i) => match body[i..].find('{') {
            Some(j) => i + j,
            None => {
                fail("No current block in response");
                return 0;
            }
        },
        None => {
            fail("No 'current' field in Open-Meteo response");
            return 0;
        }
    };
    let current_block = &body[current_start..];

    let temperature = extract_json_number(current_block, "temperature_2m").unwrap_or("0");
    let humidity = extract_json_number(current_block, "relative_humidity_2m").unwrap_or("0");
    let wind_speed = extract_json_number(current_block, "wind_speed_10m").unwrap_or("0");
    let precipitation = extract_json_number(current_block, "precipitation").unwrap_or("0");
    let weather_code_str = extract_json_number(current_block, "weather_code").unwrap_or("0");

    let weather_code: i32 = weather_code_str
        .parse()
        .unwrap_or(0);
    let conditions = wmo_code_to_condition(weather_code);

    log(
        "info",
        &format!(
            "fetch_open_meteo: temp={}C humidity={}% wind={}km/h precip={}mm conditions={}",
            temperature, humidity, wind_speed, precipitation, conditions
        ),
    );

    // 5. Return success callback with weather data
    let result = format!(
        r#"{{"action":"FetchSucceeded","params":{{"Temperature":"{}","Humidity":"{}","WindSpeed":"{}","Precipitation":"{}","Conditions":"{}"}},"success":true}}"#,
        temperature, humidity, wind_speed, precipitation, conditions
    );
    set_result(&result);
    0
}
