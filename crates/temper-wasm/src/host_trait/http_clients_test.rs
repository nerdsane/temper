use std::collections::BTreeMap;
use std::time::Duration;

use super::*;

fn secrets(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
}

#[test]
fn repeated_configuration_reuses_one_entry() {
    let cache = HttpClientCache::default();
    let secrets = secrets(&[("api_key", "one")]);
    cache.get(&secrets, Duration::from_secs(30));
    cache.get(&secrets, Duration::from_secs(30));
    assert_eq!(cache.len(), 1);
}

#[test]
fn only_ca_cert_secrets_distinguish_configurations() {
    let cache = HttpClientCache::default();
    let timeout = Duration::from_secs(30);
    cache.get(&secrets(&[("api_key", "one")]), timeout);
    cache.get(
        &secrets(&[("api_key", "two"), ("blob_endpoint", "x")]),
        timeout,
    );
    assert_eq!(cache.len(), 1, "non-CA secrets must share a client pair");

    cache.get(&secrets(&[("ca_cert:internal", "not a pem")]), timeout);
    assert_eq!(cache.len(), 2, "a private CA set needs its own client pair");
}

#[test]
fn timeout_distinguishes_configurations() {
    let cache = HttpClientCache::default();
    let secrets = BTreeMap::new();
    cache.get(&secrets, Duration::from_secs(30));
    cache.get(&secrets, Duration::from_secs(120));
    assert_eq!(cache.len(), 2);
}

#[test]
fn cache_stays_within_its_budget() {
    let cache = HttpClientCache::with_budget(2);
    let secrets = BTreeMap::new();
    for seconds in 1..=3 {
        cache.get(&secrets, Duration::from_secs(seconds));
    }
    assert_eq!(cache.len(), 2);
}
