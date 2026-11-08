use super::*;

#[test]
fn log_gives_the_count_of_seeded_secrets_and_no_names() {
    let vault = test_vault();

    let (_, log) = seed_with_log(
        &vault,
        vars(&[
            ("TEMPER_SECRET_BUILD_TOKEN", "abc"),
            ("TEMPER_SECRET_REGION", "north"),
        ]),
    );

    assert_eq!(log.lines().count(), 1, "{log}");
    assert!(log.contains("INFO"), "{log}");
    assert!(
        log.contains("seeded platform secrets from TEMPER_SECRET_ environment variables"),
        "{log}"
    );
    assert!(log.contains("count=2"), "{log}");
    assert!(!log.to_lowercase().contains("build_token"), "{log}");
    assert!(!log.to_lowercase().contains("region"), "{log}");
}

#[test]
fn no_report_log_line_or_span_contains_a_value() {
    let vault = test_vault();
    // One variable for every outcome: seeded, badly named, already set and
    // too large.
    vault.cache_platform_secret("region", "north".to_string());
    let too_large = DISTINCT_VALUE.repeat(crate::secrets::vault::MAX_SECRET_VALUE_BYTES);
    let variables = vars(&[
        ("TEMPER_SECRET_BUILD_TOKEN", DISTINCT_VALUE),
        ("TEMPER_SECRET_build_token", DISTINCT_VALUE),
        ("TEMPER_SECRET_REGION", DISTINCT_VALUE),
        ("TEMPER_SECRET_LARGE", &too_large),
    ]);

    let (report, log) = seed_with_log(&vault, variables);

    let reasons: Vec<EnvironmentSecretSkip> =
        report.skipped.iter().map(|(_, reason)| *reason).collect();
    for reason in [
        EnvironmentSecretSkip::InvalidName,
        EnvironmentSecretSkip::AlreadySet,
        EnvironmentSecretSkip::ValueTooLarge,
    ] {
        assert!(reasons.contains(&reason), "{reason:?} not exercised");
    }
    assert!(!report.seeded.is_empty());
    assert!(!log.is_empty());
    assert!(!log.contains(DISTINCT_VALUE), "{log}");
    assert!(
        !format!("{report:?}").contains(DISTINCT_VALUE),
        "{report:?}"
    );
}
