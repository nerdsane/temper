use super::*;

#[test]
fn name_the_server_already_set_keeps_its_value() {
    let vault = test_vault();
    // What start-up does for ANTHROPIC_API_KEY before the prefixed variables.
    vault.cache_platform_secret("anthropic_api_key", "from-fixed-variable".to_string());

    let (report, log) = seed_with_log(
        &vault,
        vars(&[
            ("TEMPER_SECRET_ANTHROPIC_API_KEY", "from-prefixed-variable"),
            ("TEMPER_SECRET_BUILD_TOKEN", "abc"),
        ]),
    );

    assert_eq!(
        vault.get_secret(TENANT, "anthropic_api_key"),
        Some("from-fixed-variable".into())
    );
    assert_eq!(report.seeded, vec!["build_token".to_string()]);
    assert_eq!(
        report.skipped,
        vec![(
            "TEMPER_SECRET_ANTHROPIC_API_KEY".to_string(),
            EnvironmentSecretSkip::AlreadySet
        )]
    );
    assert!(
        log.contains("variable=TEMPER_SECRET_ANTHROPIC_API_KEY "),
        "{log}"
    );
    assert!(!log.contains("from-prefixed-variable"), "{log}");
    assert!(!log.contains("from-fixed-variable"), "{log}");
}

#[test]
fn prefixed_form_of_a_fixed_name_is_seeded_when_the_server_has_not_set_it() {
    let vault = test_vault();

    let report = seed_platform_secrets_from_environment(
        &vault,
        vars(&[("TEMPER_SECRET_ANTHROPIC_API_KEY", "from-prefixed-variable")]),
    );

    assert_eq!(report.seeded, vec!["anthropic_api_key".to_string()]);
    assert_eq!(
        vault.get_secret(TENANT, "anthropic_api_key"),
        Some("from-prefixed-variable".into())
    );
}

#[test]
fn stored_tenant_secret_wins_over_a_seeded_one_for_that_tenant_only() {
    let vault = test_vault();
    // tenant-a stored its own build_token before the server started;
    // tenant-b stores one after.
    vault.cache_secret("tenant-a", "build_token", "stored-by-a".to_string());

    seed_platform_secrets_from_environment(&vault, vars(&[("TEMPER_SECRET_BUILD_TOKEN", "abc")]));
    vault.cache_secret("tenant-b", "build_token", "stored-by-b".to_string());

    assert_eq!(
        vault.get_secret("tenant-a", "build_token"),
        Some("stored-by-a".into())
    );
    assert_eq!(
        vault.get_secret("tenant-b", "build_token"),
        Some("stored-by-b".into())
    );
    assert_eq!(
        vault.get_tenant_secrets("tenant-a").get("build_token"),
        Some(&"stored-by-a".to_string())
    );
    // A tenant with no stored secret of that name reads the seeded one.
    assert_eq!(
        vault.get_secret("tenant-c", "build_token"),
        Some("abc".into())
    );

    // Removing the stored secret uncovers the seeded one again.
    assert!(vault.remove_secret("tenant-a", "build_token"));
    assert_eq!(
        vault.get_secret("tenant-a", "build_token"),
        Some("abc".into())
    );
}

#[test]
fn every_variable_is_seeded_in_name_order() {
    let vault = test_vault();
    // Past the 100 the platform layer used to hold, so the 101st and 102nd
    // used to be skipped. Listed in reverse, to show the outcome follows the
    // names and not the order the environment lists them in.
    let count = 102;
    let variables: Vec<(OsString, OsString)> = (0..count)
        .rev()
        .flat_map(|i| vars(&[(format!("TEMPER_SECRET_KEY_{i:03}").as_str(), "abc")]))
        .collect();

    let report = seed_platform_secrets_from_environment(&vault, variables);

    assert_eq!(report.seeded.len(), count);
    assert_eq!(report.seeded.first().map(String::as_str), Some("key_000"));
    assert_eq!(report.seeded.last().map(String::as_str), Some("key_101"));
    assert!(report.skipped.is_empty(), "{:?}", report.skipped);
    assert_eq!(vault.get_platform_secrets().len(), count);
}
