use base64::Engine as _;

use super::*;
use crate::genesis_install::{GenesisRegistryBundleApp, GenesisRegistryBundleFile};

#[test]
fn the_response_cap_carries_a_full_install_budget() {
    // The budget's content alone, as base64, is about 341 MiB. The old cap
    // was 128 MiB, so any bundle over about 96 MiB failed in transport even
    // though the install budget allowed it.
    let encoded_budget = MAX_GENESIS_BUNDLE_TOTAL_BYTES.div_ceil(3) * 4;
    assert!(
        MAX_BUNDLE_RESPONSE_BYTES as u64 > encoded_budget,
        "cap {MAX_BUNDLE_RESPONSE_BYTES} < encoded budget {encoded_budget}"
    );
}

#[test]
fn a_bundle_with_the_longest_names_fits_its_allowance() {
    // Paths made of bytes JSON escapes to six characters, at the longest a
    // writable path can be, and owner and app names at their longest.
    let path: String = std::iter::repeat_n('\u{1f}', 4095).collect();
    let id = "o".repeat(128);
    let mut content_bytes = 0u64;
    let apps: Vec<_> = (0..2)
        .map(|app| GenesisRegistryBundleApp {
            owner: id.clone(),
            name: format!("{app}{}", "n".repeat(127)),
            version_hash: "a".repeat(40),
            files: (0..3)
                .map(|file| {
                    let bytes = vec![0xffu8; 10 + file];
                    content_bytes += bytes.len() as u64;
                    GenesisRegistryBundleFile {
                        path: path.clone(),
                        content_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
                    }
                })
                .collect(),
        })
        .collect();
    let bundle = GenesisRegistryBundleResponse {
        app_ref: format!("{id}/{}@{}", "n".repeat(128), "a".repeat(40)),
        registry_tenant: "t".repeat(1024),
        apps,
    };
    let encoded = serde_json::to_vec(&bundle).expect("serialize bundle");
    let allowance = bundle_response_allowance(content_bytes, 6, 2);
    assert!(
        encoded.len() <= allowance,
        "{} bytes > allowance {allowance}",
        encoded.len()
    );
}
