//! Which spec a declared version names.

use super::*;

/// A stand-in for a registered content hash: 64 lowercase hex characters.
const REGISTERED: &str = "0f1e2d3c4b5a69788796a5b4c3d2e1f00f1e2d3c4b5a69788796a5b4c3d2e1f0";

/// Another spec's hash, sharing no prefix with [`REGISTERED`].
const OTHER: &str = "abcdef01234567890abcdef01234567890abcdef01234567890abcdef0123456";

fn classify(declared: &str) -> PinMatch {
    classify_pin(declared, "Order", REGISTERED)
}

#[test]
fn the_bare_hash_names_the_registered_spec() {
    assert_eq!(classify(REGISTERED), PinMatch::Registered);
}

#[test]
fn the_algorithm_qualified_hash_names_the_registered_spec() {
    assert_eq!(
        classify(&format!("sha256:{REGISTERED}")),
        PinMatch::Registered
    );
}

#[test]
fn another_spec_s_hash_names_another_version() {
    assert_eq!(classify(OTHER), PinMatch::OtherVersion);
    assert_eq!(classify("sha256:v1-long-gone"), PinMatch::OtherVersion);
}

#[test]
fn an_entity_qualified_prefix_names_the_registered_spec() {
    // The pin katagami stamps: the entity, then a short digest.
    let prefix = &REGISTERED[..MIN_PIN_DIGEST_HEX];
    assert_eq!(
        classify(&format!("Order@sha256:{prefix}")),
        PinMatch::Registered
    );
}

#[test]
fn an_entity_qualified_full_hash_names_the_registered_spec() {
    assert_eq!(
        classify(&format!("Order@sha256:{REGISTERED}")),
        PinMatch::Registered
    );
}

#[test]
fn a_prefix_is_matched_without_regard_to_case() {
    let prefix = REGISTERED[..20].to_ascii_uppercase();
    assert_eq!(
        classify(&format!("Order@sha256:{prefix}")),
        PinMatch::Registered
    );
}

#[test]
fn an_entity_qualified_prefix_of_another_spec_names_another_version() {
    let prefix = &OTHER[..MIN_PIN_DIGEST_HEX];
    assert_eq!(
        classify(&format!("Order@sha256:{prefix}")),
        PinMatch::OtherVersion
    );
}

#[test]
fn a_digest_truncated_past_the_minimum_names_nothing() {
    // The ambiguity guard: below the floor a digest stops identifying one
    // spec, so it is refused rather than resolved to whatever it prefixes.
    for length in 1..MIN_PIN_DIGEST_HEX {
        let prefix = &REGISTERED[..length];
        assert_eq!(
            classify(&format!("Order@sha256:{prefix}")),
            PinMatch::DigestTooShort,
            "a {length}-character digest must be refused, not resolved"
        );
    }
}

#[test]
fn the_shortest_accepted_digest_is_exactly_the_floor() {
    let at_floor = &REGISTERED[..MIN_PIN_DIGEST_HEX];
    let below_floor = &REGISTERED[..MIN_PIN_DIGEST_HEX - 1];
    assert_eq!(
        classify(&format!("Order@sha256:{at_floor}")),
        PinMatch::Registered
    );
    assert_eq!(
        classify(&format!("Order@sha256:{below_floor}")),
        PinMatch::DigestTooShort
    );
}

#[test]
fn a_bare_prefix_is_not_accepted_as_a_prefix() {
    // Nothing in an unqualified digest says its author meant a prefix rather
    // than a different spec, so it stays an exact comparison.
    let prefix = &REGISTERED[..MIN_PIN_DIGEST_HEX];
    assert_eq!(classify(prefix), PinMatch::OtherVersion);
    assert_eq!(
        classify(&format!("sha256:{prefix}")),
        PinMatch::OtherVersion
    );
}

#[test]
fn a_pin_for_another_entity_is_refused_on_its_own_terms() {
    // Same digest, wrong actor: reported as the wrong entity rather than as
    // this actor having run under some other version.
    let prefix = &REGISTERED[..MIN_PIN_DIGEST_HEX];
    assert_eq!(
        classify(&format!("Invoice@sha256:{prefix}")),
        PinMatch::WrongEntity
    );
    assert_eq!(
        classify(&format!("Invoice@sha256:{REGISTERED}")),
        PinMatch::WrongEntity
    );
}

#[test]
fn a_qualified_non_digest_names_a_version_this_kernel_cannot_resolve() {
    assert_eq!(
        classify("Order@sha256:v1-long-gone"),
        PinMatch::OtherVersion
    );
}

#[test]
fn a_digest_longer_than_sha256_names_another_version() {
    assert_eq!(
        classify(&format!("Order@sha256:{REGISTERED}0")),
        PinMatch::OtherVersion
    );
}

#[test]
fn surrounding_whitespace_does_not_change_what_a_pin_names() {
    let prefix = &REGISTERED[..MIN_PIN_DIGEST_HEX];
    assert_eq!(
        classify(&format!(" Order @ sha256:{prefix} ")),
        PinMatch::Registered
    );
}

fn agree(left: &str, right: &str) -> bool {
    declare_same_spec(left, right, "Order", REGISTERED)
}

#[test]
fn two_spellings_of_one_version_agree() {
    assert!(agree(REGISTERED, &format!("sha256:{REGISTERED}")));
    assert!(agree(
        &format!("Order@sha256:{REGISTERED}"),
        &format!("sha256:{REGISTERED}")
    ));
}

#[test]
fn a_prefix_pin_agrees_with_the_full_hash_it_prefixes() {
    let prefix = &REGISTERED[..MIN_PIN_DIGEST_HEX];
    assert!(
        agree(&format!("Order@sha256:{prefix}"), REGISTERED),
        "both resolve to the registered spec, so neither contradicts the other"
    );
}

#[test]
fn two_spellings_of_a_version_that_is_gone_still_agree() {
    // Neither resolves, but they do not disagree with each other, and saying
    // they do would blame the request for a spec that was simply replaced.
    assert!(agree("sha256:v1-long-gone", "v1-long-gone"));
}

#[test]
fn declarations_naming_different_versions_disagree() {
    assert!(!agree(REGISTERED, OTHER));
    assert!(!agree("sha256:v1-long-gone", REGISTERED));
}

#[test]
fn a_prefix_pin_for_another_entity_disagrees() {
    let prefix = &REGISTERED[..MIN_PIN_DIGEST_HEX];
    assert!(
        !agree(&format!("Invoice@sha256:{prefix}"), REGISTERED),
        "a pin naming another actor cannot stand in for this run's version"
    );
}
