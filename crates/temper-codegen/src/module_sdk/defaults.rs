use temper_spec::csdl::CsdlDocument;
use temper_wasm_sdk::data::{ManifestPropertyV1, ManifestValueSourceV1};

use super::{ModuleSdkCodegenError, names::rust_field_name, schema_helpers::enum_members};

pub(super) fn manifest_property(
    csdl: &CsdlDocument,
    canonical_name: &str,
    type_name: &str,
    nullable: bool,
    declared_default: Option<&str>,
    source: ManifestValueSourceV1,
) -> Result<ManifestPropertyV1, ModuleSdkCodegenError> {
    let enum_members = enum_members(csdl, type_name);
    let default_value = declared_default
        .map(|value| canonical_default(canonical_name, type_name, &enum_members, value))
        .transpose()?;
    Ok(ManifestPropertyV1 {
        canonical_name: canonical_name.into(),
        generated_name: rust_field_name(canonical_name),
        type_name: type_name.into(),
        nullable,
        source,
        default_value,
        enum_members,
    })
}

fn canonical_default(
    symbol: &str,
    type_name: &str,
    enum_members: &[String],
    value: &str,
) -> Result<serde_json::Value, ModuleSdkCodegenError> {
    let invalid = || ModuleSdkCodegenError::InvalidDefault {
        symbol: symbol.into(),
        type_name: type_name.into(),
        value: value.into(),
    };
    if !enum_members.is_empty() {
        return enum_members
            .iter()
            .any(|member| member == value)
            .then(|| serde_json::Value::String(value.into()))
            .ok_or_else(invalid);
    }
    let canonical = match type_name {
        "Edm.Boolean" if value.eq_ignore_ascii_case("true") => serde_json::Value::Bool(true),
        "Edm.Boolean" if value.eq_ignore_ascii_case("false") => serde_json::Value::Bool(false),
        "Edm.Boolean" => return Err(invalid()),
        "Edm.Byte" if unsigned_integer_lexical(value, 3) => {
            serde_json::Value::Number(value.parse::<u8>().map_err(|_| invalid())?.into())
        }
        "Edm.Int16" if signed_integer_lexical(value, 5) => {
            serde_json::Value::Number(value.parse::<i16>().map_err(|_| invalid())?.into())
        }
        "Edm.Int32" if signed_integer_lexical(value, 10) => {
            serde_json::Value::Number(value.parse::<i32>().map_err(|_| invalid())?.into())
        }
        "Edm.Int64" if signed_integer_lexical(value, 19) => {
            serde_json::Value::Number(value.parse::<i64>().map_err(|_| invalid())?.into())
        }
        "Edm.Byte" | "Edm.Int16" | "Edm.Int32" | "Edm.Int64" => return Err(invalid()),
        "Edm.Single" if decimal_lexical(value) => {
            let number = value.parse::<f32>().map_err(|_| invalid())?;
            if !number.is_finite() {
                return Err(invalid());
            }
            serde_json::Number::from_f64(f64::from(number))
                .map(serde_json::Value::Number)
                .ok_or_else(invalid)?
        }
        "Edm.Double" if decimal_lexical(value) => {
            let number = value.parse::<f64>().map_err(|_| invalid())?;
            if !number.is_finite() {
                return Err(invalid());
            }
            serde_json::Number::from_f64(number)
                .map(serde_json::Value::Number)
                .ok_or_else(invalid)?
        }
        "Edm.Single" | "Edm.Double" => return Err(invalid()),
        "Edm.Decimal" if decimal_lexical(value) => serde_json::Value::String(value.into()),
        "Edm.Decimal" => return Err(invalid()),
        "Edm.Guid" => {
            if !guid_lexical(value) || uuid::Uuid::parse_str(value).is_err() {
                return Err(invalid());
            }
            serde_json::Value::String(value.into())
        }
        "Edm.String" => serde_json::Value::String(value.into()),
        "Edm.Binary" if binary_lexical(value) => serde_json::Value::String(value.into()),
        "Edm.Binary" => return Err(invalid()),
        // The closed module ABI does not yet support these Edm primitives. Reject
        // their defaults instead of persisting an unvalidated string.
        _ if type_name.starts_with("Edm.") => return Err(invalid()),
        // CSDL references and named scalar aliases cross the module ABI as strings.
        _ => serde_json::Value::String(value.into()),
    };
    Ok(canonical)
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
    parts.next().is_none()
        && !whole.is_empty()
        && whole.bytes().all(|byte| byte.is_ascii_digit())
        && fraction.is_none_or(|digits| {
            !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
        })
}

fn unsigned_integer_lexical(value: &str, digit_budget: usize) -> bool {
    !value.is_empty()
        && value.len() <= digit_budget
        && value.bytes().all(|byte| byte.is_ascii_digit())
}

fn signed_integer_lexical(value: &str, digit_budget: usize) -> bool {
    let digits = value.strip_prefix(['+', '-']).unwrap_or(value);
    unsigned_integer_lexical(digits, digit_budget)
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
                && unpadded
                    .as_bytes()
                    .last()
                    .is_some_and(|byte| matches!(byte, b'A' | b'Q' | b'g' | b'w'))
        }
        3 => {
            padding <= 1
                && unpadded.as_bytes().last().is_some_and(|byte| {
                    matches!(
                        byte,
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
                })
        }
        _ => false,
    }
}
