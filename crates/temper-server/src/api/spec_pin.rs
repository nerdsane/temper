//! How a declared spec version names a registered spec.
//!
//! A conformance report only means something against the spec the run executed
//! under, so the version a run declares has to be matched against the one the
//! kernel has registered. Harnesses write that version in more than one form:
//!
//! - `0f1e2d…` — the bare content hash, 64 hex characters;
//! - `sha256:0f1e2d…` — the same hash, algorithm-qualified;
//! - `Order@sha256:0f1e2d3c4b5a` — the kernel's own vocabulary: the entity the
//!   spec governs, then a digest that may be **truncated**, the way a short
//!   object id names a commit.
//!
//! The first two are matched exactly, as they always were. The third is the
//! addition: the entity qualifier selects the spec — Temper registers exactly
//! one spec per (tenant, entity type) — and the digest then has to prefix that
//! spec's hash.
//!
//! # Why a truncated digest has to carry the entity
//!
//! Without it a short digest names nothing in particular: it would have to be
//! searched for across every spec in the tenant, and a collision would judge a
//! run by another actor's rules. With it there is only ever one spec for the
//! digest to agree with, so a prefix identifies as precisely as the full hash
//! does. A bare short digest is therefore refused rather than searched.
//!
//! # What a prefix does not fix
//!
//! It does not make a re-serialized spec match. sha256 avalanches: normalising
//! line endings or re-emitting the TOML changes the whole digest, its first
//! twelve characters included, so a producer that recomputes the hash from a
//! rewritten file disagrees at any length. The cure for that is for producers
//! to read the registered digest off `GET /observe/specs/{entity}` rather than
//! computing one themselves.

/// Shortest digest accepted in an entity-qualified pin, in hex characters.
///
/// Twelve hex characters is 48 bits — the length git settled on for short
/// object ids in large repositories, and far past the point where two specs in
/// one tenant collide by accident.
pub(crate) const MIN_PIN_DIGEST_HEX: usize = 12;

/// Length of a full sha256 digest in hex characters.
const SHA256_HEX_LEN: usize = 64;

/// What a declared version names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PinMatch {
    /// The registered spec.
    Registered,
    /// A spec, but not the registered one.
    OtherVersion,
    /// A spec belonging to a different entity than the one being checked.
    WrongEntity,
    /// Nothing in particular: the digest is truncated past the point where it
    /// identifies one spec.
    DigestTooShort,
}

/// A spec version as a harness or a request wrote it.
struct DeclaredVersion<'a> {
    /// The entity the pin names, when it is qualified with one.
    entity: Option<&'a str>,
    /// The digest as written: a full hash, or a prefix of one.
    digest: &'a str,
}

impl<'a> DeclaredVersion<'a> {
    fn parse(declared: &'a str) -> Self {
        let (entity, rest) = match declared.split_once('@') {
            Some((entity, rest)) => (Some(entity.trim()), rest),
            None => (None, declared),
        };
        let rest = rest.trim();
        Self {
            entity,
            digest: rest.strip_prefix("sha256:").unwrap_or(rest),
        }
    }

    fn is_hex(&self) -> bool {
        !self.digest.is_empty() && self.digest.chars().all(|c| c.is_ascii_hexdigit())
    }
}

/// Decide what `declared` names, given the spec registered for `entity_type`.
pub(crate) fn classify_pin(declared: &str, entity_type: &str, registered_hash: &str) -> PinMatch {
    let declared = DeclaredVersion::parse(declared);

    let Some(entity) = declared.entity else {
        // Unqualified: matched exactly, the way it always was. A bare digest
        // that happens to prefix the registered hash is not accepted — nothing
        // in it says the author meant a prefix rather than a different spec.
        return if declared.digest == registered_hash {
            PinMatch::Registered
        } else {
            PinMatch::OtherVersion
        };
    };

    // The qualifier selects the spec before the digest is read, so a pin for
    // another actor is refused on its own terms rather than reported as this
    // actor having run under some other version.
    if entity != entity_type {
        return PinMatch::WrongEntity;
    }
    if !declared.is_hex() {
        // A qualified pin carrying something that is not a digest at all — a
        // tag, a release name — names a version this kernel cannot resolve.
        return PinMatch::OtherVersion;
    }
    if declared.digest.len() < MIN_PIN_DIGEST_HEX {
        return PinMatch::DigestTooShort;
    }
    if declared.digest.len() > SHA256_HEX_LEN {
        return PinMatch::OtherVersion;
    }
    if hex_prefix_of(registered_hash, declared.digest) {
        PinMatch::Registered
    } else {
        PinMatch::OtherVersion
    }
}

/// Whether two declarations name the same spec.
///
/// Two spellings agree when they are the same version written two ways, or
/// when both resolve to the registered spec. The written comparison runs
/// first, so two spellings of a version that is no longer registered still
/// read as agreement rather than as the request contradicting the run.
pub(crate) fn declare_same_spec(
    left: &str,
    right: &str,
    entity_type: &str,
    registered_hash: &str,
) -> bool {
    if same_written_version(
        &DeclaredVersion::parse(left),
        &DeclaredVersion::parse(right),
    ) {
        return true;
    }
    classify_pin(left, entity_type, registered_hash) == PinMatch::Registered
        && classify_pin(right, entity_type, registered_hash) == PinMatch::Registered
}

/// Whether two declarations are one version written two ways.
///
/// `sha256:0f1e…` and a bare `0f1e…` are one version, and so is the same
/// digest with and without an entity qualifier — the digest is what identifies
/// a spec, and a qualifier present on only one side says nothing against it.
fn same_written_version(left: &DeclaredVersion<'_>, right: &DeclaredVersion<'_>) -> bool {
    if left.digest != right.digest {
        return false;
    }
    match (left.entity, right.entity) {
        (Some(left), Some(right)) => left == right,
        _ => true,
    }
}

/// Whether `prefix` is a leading run of `full`, comparing hex without regard
/// to case — a digest is the same value however a producer cased it.
fn hex_prefix_of(full: &str, prefix: &str) -> bool {
    full.len() >= prefix.len()
        && full.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
}

#[cfg(test)]
#[path = "spec_pin_test.rs"]
mod spec_pin_test;
