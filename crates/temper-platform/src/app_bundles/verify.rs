use std::path::Path;

use super::types::CanonicalBundleManifestV1;
use crate::os_apps::{load_app_bundle, read_app_manifest};

pub(super) fn verify_materialized_bundle(
    view: &Path,
    manifest: &CanonicalBundleManifestV1,
) -> Result<(), String> {
    for app in &manifest.apps {
        let app_path = view.join(&app.name);
        let source_manifest = read_app_manifest(&app_path)
            .ok_or_else(|| format!("materialized app '{}' has no valid app.toml", app.name))?;
        let mut declared_dependencies = source_manifest
            .dependencies
            .iter()
            .map(|dependency| dependency_name(dependency).to_string())
            .collect::<Vec<_>>();
        declared_dependencies.sort();
        declared_dependencies.dedup();
        if source_manifest.name != app.name
            || source_manifest.version != app.version
            || declared_dependencies != app.dependencies
        {
            return Err(format!(
                "canonical metadata for '{}' does not match its app.toml",
                app.name
            ));
        }
        let bundle = load_app_bundle(&app_path)
            .ok_or_else(|| format!("materialized app '{}' could not be parsed", app.name))?;
        if let Some(csdl) = &bundle.csdl {
            temper_spec::csdl::parse_csdl(csdl)
                .map_err(|error| format!("CSDL verification failed for '{}': {error}", app.name))?;
        }
        for (entity_type, source) in &bundle.specs {
            let result = temper_verify::cascade::VerificationCascade::from_ioa(source)
                .with_sim_seeds(5)
                .with_prop_test_cases(100)
                .with_fail_fast()
                .run();
            if !result.all_passed {
                return Err(format!(
                    "verification cascade rejected '{}.{}'",
                    app.name, entity_type
                ));
            }
        }
    }
    Ok(())
}

fn dependency_name(raw: &str) -> &str {
    let unpinned = raw
        .trim()
        .split_once('@')
        .map_or(raw.trim(), |(left, _)| left);
    unpinned.rsplit_once('/').map_or(unpinned, |(_, name)| name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_bundles::types::{CanonicalAppManifest, CanonicalBundleManifestV1};

    #[test]
    fn verification_rejects_manifest_metadata_that_disagrees_with_app_toml() {
        let view = tempfile::tempdir().unwrap();
        let app_path = view.path().join("sample");
        std::fs::create_dir_all(&app_path).unwrap();
        std::fs::write(
            app_path.join("app.toml"),
            "name = \"different\"\nversion = \"1.0.0\"\n",
        )
        .unwrap();
        std::fs::write(app_path.join("APP.md"), "# Sample\n").unwrap();
        let manifest = CanonicalBundleManifestV1 {
            schema_version: 1,
            root_app: "sample".to_string(),
            apps: vec![CanonicalAppManifest {
                name: "sample".to_string(),
                version: "1.0.0".to_string(),
                dependencies: Vec::new(),
                files: Vec::new(),
            }],
            bundle_digest: String::new(),
        };
        let error = verify_materialized_bundle(view.path(), &manifest).unwrap_err();
        assert!(
            error.contains("does not match"),
            "unexpected error: {error}"
        );
    }
}
