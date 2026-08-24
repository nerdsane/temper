use sha2::{Digest, Sha256};

use super::{AppBundle, AppEntry, OsAppBundleDigest, catalog, get_os_app};

fn app_entry(app_name: &str) -> Option<AppEntry> {
    let catalog = catalog().read().expect("OS app catalog lock poisoned");
    catalog
        .entries
        .iter()
        .find(|entry| entry.name == app_name)
        .cloned()
}

fn digest_bytes(parts: &[(&str, Vec<u8>)]) -> String {
    let mut hasher = Sha256::new();
    for (name, bytes) in parts {
        hasher.update(name.as_bytes());
        hasher.update([0]);
        hasher.update(bytes);
        hasher.update([0xff]);
    }
    format!("sha256:{:x}", hasher.finalize())
}

fn digest_named_parts(parts: &[(String, Vec<u8>)]) -> String {
    let parts = parts
        .iter()
        .map(|(name, bytes)| (name.as_str(), bytes.clone()))
        .collect::<Vec<_>>();
    digest_bytes(&parts)
}

pub(super) fn digest_app_bundle(app_name: &str, bundle: &AppBundle) -> OsAppBundleDigest {
    let entry = app_entry(app_name);
    let app_version = entry
        .as_ref()
        .map(|entry| entry.version.clone())
        .unwrap_or_else(|| "0.1.0".to_string());

    digest_app_bundle_with_version(
        app_name,
        &app_version,
        entry.as_ref().and_then(|entry| entry.app_guide.as_deref()),
        bundle,
    )
}

pub(crate) fn digest_app_bundle_with_version(
    app_name: &str,
    app_version: &str,
    app_guide: Option<&str>,
    bundle: &AppBundle,
) -> OsAppBundleDigest {
    let mut spec_parts = Vec::new();
    for (entity_type, ioa_source) in &bundle.specs {
        spec_parts.push((
            format!("spec:{entity_type}"),
            ioa_source.as_bytes().to_vec(),
        ));
    }
    if let Some(csdl) = &bundle.csdl {
        spec_parts.push(("csdl".to_string(), csdl.as_bytes().to_vec()));
    }
    if let Some(cross_invariants) = &bundle.cross_invariants_toml {
        spec_parts.push((
            "cross-invariants".to_string(),
            cross_invariants.as_bytes().to_vec(),
        ));
    }
    spec_parts.sort_by(|a, b| a.0.cmp(&b.0));

    let mut policy_parts: Vec<(String, Vec<u8>)> = bundle
        .cedar_policy_sources
        .iter()
        .map(|source| {
            (
                format!("policy:{}", source.relative_path),
                source.text.as_bytes().to_vec(),
            )
        })
        .collect();
    policy_parts.sort_by(|a, b| a.0.cmp(&b.0));

    let mut wasm_parts = Vec::new();
    for (module_name, wasm_bytes) in &bundle.wasm_modules {
        wasm_parts.push((format!("wasm:{module_name}"), wasm_bytes.clone()));
    }
    for (module_name, config) in &bundle.wasm_module_configs {
        // The binding is derived from this digest, so exclude it from the
        // immutable closure input while retaining the grant declaration.
        let mut closure_config = config.clone();
        closure_config.data_binding = None;
        let config_bytes = serde_json::to_vec(&closure_config).unwrap_or_default();
        wasm_parts.push((format!("wasm-config:{module_name}"), config_bytes));
    }
    wasm_parts.sort_by(|a, b| a.0.cmp(&b.0));

    let mut content_parts = Vec::new();
    if let Some(app_guide) = app_guide {
        content_parts.push(("APP.md".to_string(), app_guide.as_bytes().to_vec()));
    }
    for agent in &bundle.agents {
        content_parts.push((
            format!("agent:{}:content", agent.name),
            agent.content.as_bytes().to_vec(),
        ));
    }
    for skill in &bundle.skills {
        content_parts.push((
            format!(
                "skill:{}:{}",
                skill.agent_name.as_deref().unwrap_or("_system"),
                skill.name
            ),
            skill.content.as_bytes().to_vec(),
        ));
        for companion in &skill.companion_files {
            content_parts.push((
                format!(
                    "skill-companion:{}:{}:{}",
                    skill.agent_name.as_deref().unwrap_or("_system"),
                    skill.name,
                    companion.name
                ),
                companion.content.clone(),
            ));
        }
    }
    for file in &bundle.system_files {
        content_parts.push((
            format!("system:{}", file.relative_path),
            file.content.clone(),
        ));
    }
    for adr in &bundle.adrs {
        content_parts.push((
            format!("adr:{}", adr.file_name),
            adr.content.as_bytes().to_vec(),
        ));
    }
    content_parts.sort_by(|a, b| a.0.cmp(&b.0));

    let mut seed_parts = Vec::new();
    for seed in &bundle.seed_instances {
        seed_parts.push((
            format!(
                "seed:{}:{}",
                seed.entity_type,
                seed.id.as_deref().unwrap_or("_generated")
            ),
            serde_json::to_vec(seed).unwrap_or_default(),
        ));
    }
    seed_parts.sort_by(|a, b| a.0.cmp(&b.0));

    let spec_digest = digest_named_parts(&spec_parts);
    let policy_digest = digest_named_parts(&policy_parts);
    let wasm_digest = digest_named_parts(&wasm_parts);
    let content_digest = digest_named_parts(&content_parts);
    let seed_digest = digest_named_parts(&seed_parts);
    let bundle_digest = digest_bytes(&[
        ("app_name", app_name.as_bytes().to_vec()),
        ("app_version", app_version.as_bytes().to_vec()),
        ("spec", spec_digest.as_bytes().to_vec()),
        ("policy", policy_digest.as_bytes().to_vec()),
        ("wasm", wasm_digest.as_bytes().to_vec()),
        ("content", content_digest.as_bytes().to_vec()),
        ("seed", seed_digest.as_bytes().to_vec()),
    ]);

    OsAppBundleDigest {
        app_name: app_name.to_string(),
        app_version: app_version.to_string(),
        bundle_digest,
        spec_digest,
        policy_digest,
        wasm_digest,
        content_digest,
        seed_digest,
    }
}

/// Compute the current bundle digest for an app in the catalog.
pub fn os_app_bundle_digest(app_name: &str) -> Option<OsAppBundleDigest> {
    get_os_app(app_name).map(|bundle| digest_app_bundle(app_name, &bundle))
}
