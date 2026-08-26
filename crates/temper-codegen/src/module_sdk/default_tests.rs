use temper_spec::csdl::parse_csdl;

use super::ModuleSdkCodegenError;
use super::tests::{CSDL, generate_module_sdk, grant};

#[test]
fn generation_preserves_typed_canonical_defaults() {
    let source = CSDL.replace(
        "<Property Name=\"Status\" Type=\"Edm.String\" Nullable=\"false\" DefaultValue=\"Open\"/>",
        concat!(
            "<Property Name=\"Status\" Type=\"Edm.String\" Nullable=\"false\" DefaultValue=\"Open\"/>",
            "<Property Name=\"FailureReason\" Type=\"Edm.String\" Nullable=\"false\" DefaultValue=\"\"/>",
            "<Property Name=\"Label\" Type=\"Edm.String\" Nullable=\"false\" DefaultValue=\"unknown\"/>",
            "<Property Name=\"AttemptCount\" Type=\"Edm.Int64\" Nullable=\"false\" DefaultValue=\"0\"/>",
            "<Property Name=\"Enabled\" Type=\"Edm.Boolean\" Nullable=\"false\" DefaultValue=\"false\"/>",
            "<Property Name=\"DefaultOutcome\" Type=\"Temper.App.Outcome\" Nullable=\"false\" DefaultValue=\"Accepted\"/>",
            "<Property Name=\"Note\" Type=\"Edm.String\" Nullable=\"true\"/>"
        ),
    );
    let generated = generate_module_sdk(
        &parse_csdl(&source).unwrap(),
        "worker",
        "closure",
        "closure",
        "artifact",
        grant(),
    )
    .unwrap();
    let properties = &generated.manifest.entities[0].properties;
    let default = |name: &str| {
        properties
            .iter()
            .find(|property| property.canonical_name == name)
            .and_then(|property| property.default_value.as_ref())
    };
    assert_eq!(default("FailureReason"), Some(&serde_json::json!("")));
    assert_eq!(default("Label"), Some(&serde_json::json!("unknown")));
    assert_eq!(default("AttemptCount"), Some(&serde_json::json!(0)));
    assert_eq!(default("Enabled"), Some(&serde_json::json!(false)));
    assert_eq!(
        default("DefaultOutcome"),
        Some(&serde_json::json!("Accepted"))
    );
    assert!(default("Note").is_none());
    assert!(generated.source.contains("pub failure_reason: String"));
    assert!(generated.source.contains("pub note: Option<String>"));
}

#[test]
fn generation_rejects_invalid_scalar_and_enum_defaults() {
    for (type_name, value) in [
        ("Edm.Boolean", "not-a-boolean"),
        ("Edm.Byte", "+1"),
        ("Edm.Byte", "0001"),
        ("Edm.Int16", "000001"),
        ("Edm.Int32", "00000000001"),
        ("Edm.Int64", "00000000000000000001"),
        ("Edm.Int64", "1.5"),
        ("Edm.Binary", "not+base64url"),
        ("Edm.DateTimeOffset", "2015-02-18 23:59:59Z"),
        ("Edm.DateTimeOffset", "2015-02-18T23:59:59.1234567890123Z"),
        ("Edm.DateTimeOffset", "2015-02-18T23:59Z"),
        ("Edm.Date", "not-a-date"),
        ("Edm.Duration", "P1D"),
        ("Edm.TimeOfDay", "12:30:00"),
        ("Temper.App.Outcome", "Unknown"),
    ] {
        let source = CSDL.replace(
            "<Property Name=\"Status\" Type=\"Edm.String\" Nullable=\"false\" DefaultValue=\"Open\"/>",
            &format!(
                "<Property Name=\"Status\" Type=\"Edm.String\" Nullable=\"false\" DefaultValue=\"Open\"/><Property Name=\"Invalid\" Type=\"{type_name}\" Nullable=\"false\" DefaultValue=\"{value}\"/>"
            ),
        );
        assert!(matches!(
            generate_module_sdk(
                &parse_csdl(&source).unwrap(),
                "worker",
                "closure",
                "closure",
                "artifact",
                grant(),
            ),
            Err(ModuleSdkCodegenError::InvalidDefault { symbol, .. }) if symbol == "Invalid"
        ));
    }
}

#[test]
fn generation_accepts_supported_odata_primitive_lexicals() {
    for (type_name, value, expected) in [
        ("Edm.Boolean", "TRUE", serde_json::json!(true)),
        ("Edm.Boolean", "False", serde_json::json!(false)),
        ("Edm.Byte", "001", serde_json::json!(1)),
        ("Edm.Int16", "+0001", serde_json::json!(1)),
        ("Edm.Decimal", "+1", serde_json::json!("+1")),
        ("Edm.Decimal", "01", serde_json::json!("01")),
        ("Edm.Decimal", "1e2", serde_json::json!("1e2")),
        ("Edm.Decimal", "1E2", serde_json::json!("1E2")),
        (
            "Edm.Guid",
            "018F1F80-7B2D-7000-8000-000000000001",
            serde_json::json!("018F1F80-7B2D-7000-8000-000000000001"),
        ),
        ("Edm.Binary", "AQ==", serde_json::json!("AQ==")),
        ("Edm.Binary", "AQ", serde_json::json!("AQ")),
    ] {
        let source = CSDL.replace(
            "<Property Name=\"Status\" Type=\"Edm.String\" Nullable=\"false\" DefaultValue=\"Open\"/>",
            &format!(
                "<Property Name=\"Status\" Type=\"Edm.String\" Nullable=\"false\" DefaultValue=\"Open\"/><Property Name=\"Valid\" Type=\"{type_name}\" Nullable=\"false\" DefaultValue=\"{value}\"/>"
            ),
        );
        let generated = generate_module_sdk(
            &parse_csdl(&source).unwrap(),
            "worker",
            "closure",
            "closure",
            "artifact",
            grant(),
        )
        .unwrap();
        let actual = generated.manifest.entities[0]
            .properties
            .iter()
            .find(|property| property.canonical_name == "Valid")
            .and_then(|property| property.default_value.as_ref());
        assert_eq!(actual, Some(&expected), "{type_name} {value}");
    }
}
