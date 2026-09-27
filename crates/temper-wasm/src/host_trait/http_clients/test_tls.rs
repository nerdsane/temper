//! Test-only default TLS preparation; normal production builders are unchanged.
//!
//! Native-tls-alpn is not enabled in the pinned feature graph. TLS defaults match
//! its TLS 1.2 minimum, system roots, SNI and hostname/certificate verification.
//! This freezes default root-store contents at first use within the test process.
//! Tests that rotate system roots must bypass it, as valid custom roots do.

use std::sync::OnceLock;

pub(super) fn connector() -> &'static native_tls::TlsConnector {
    static CONNECTOR: OnceLock<native_tls::TlsConnector> = OnceLock::new();
    CONNECTOR
        .get_or_init(|| native_tls::TlsConnector::new().expect("unit-test default TLS preparation"))
}

#[cfg(test)]
mod tests;
