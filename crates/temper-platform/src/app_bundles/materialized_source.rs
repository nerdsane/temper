use std::collections::BTreeSet;
use std::path::Path;

use base64::Engine as _;

use super::MAX_BUNDLE_APPS;
use super::types::{BundleBlob, CanonicalAppManifest, CanonicalBundleManifestV1};
use super::workspace::{
    BuildState, collect_files, dependency_name, digest_app_records, digest_manifest_records,
    read_manifest,
};

pub(crate) fn build_materialized_source_bundle(
    source_root: &Path,
    root_app: &str,
    app_names: &[String],
) -> Result<(CanonicalBundleManifestV1, Vec<BundleBlob>), String> {
    let source_root = source_root.canonicalize().map_err(|error| {
        format!(
            "canonicalize source closure '{}': {error}",
            source_root.display()
        )
    })?;
    let names = app_names.iter().cloned().collect::<BTreeSet<_>>();
    if names.is_empty() || names.len() > MAX_BUNDLE_APPS || !names.contains(root_app) {
        return Err("materialized source closure has an invalid app set".to_string());
    }

    let mut state = BuildState::default();
    for expected_name in &names {
        let app_dir = source_root.join(expected_name);
        let manifest = read_manifest(&app_dir)?;
        if manifest.name != *expected_name {
            return Err(format!(
                "source directory '{}' declares app '{}'",
                expected_name, manifest.name
            ));
        }
        let mut dependencies = manifest
            .dependencies
            .iter()
            .map(|dependency| dependency_name(dependency))
            .collect::<Result<Vec<_>, _>>()?;
        dependencies.sort();
        dependencies.dedup();
        for dependency in &dependencies {
            if !names.contains(dependency) {
                return Err(format!(
                    "source app '{}' references dependency '{}' outside its closure",
                    manifest.name, dependency
                ));
            }
        }
        let files = collect_files(&app_dir, &mut state)?;
        state.app_digests.insert(
            manifest.name.clone(),
            digest_app_records(&manifest.name, &manifest.version, &dependencies, &files),
        );
        state.apps.insert(
            manifest.name.clone(),
            CanonicalAppManifest {
                name: manifest.name,
                version: manifest.version,
                dependencies,
                files,
            },
        );
    }

    let apps = state.apps.into_values().collect::<Vec<_>>();
    let bundle_digest = digest_manifest_records(root_app, &apps);
    let manifest = CanonicalBundleManifestV1 {
        schema_version: 1,
        root_app: root_app.to_string(),
        apps,
        bundle_digest,
    };
    let blobs = state
        .blobs
        .into_iter()
        .map(|(digest, bytes)| BundleBlob {
            digest,
            content_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
        })
        .collect();
    Ok((manifest, blobs))
}
