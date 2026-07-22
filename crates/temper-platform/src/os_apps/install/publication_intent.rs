use super::*;

pub(in crate::os_apps) fn append_installed_app_record_intent(
    components: &mut Vec<(String, Vec<u8>)>,
    record: &InstalledAppRecord,
) {
    for (name, value) in [
        ("tenant", record.tenant.as_str()),
        ("app_name", record.app_name.as_str()),
        ("source_kind", record.source_kind.as_str()),
        ("app_ref", record.app_ref.as_str()),
        ("version_hash", record.version_hash.as_str()),
        ("pinned_version_hash", record.pinned_version_hash.as_str()),
        ("current_version_hash", record.current_version_hash.as_str()),
        ("follow_policy", record.follow_policy.as_str()),
        ("closure_id", record.closure_id.as_str()),
        ("registry_url", record.registry_url.as_str()),
        ("registry_tenant", record.registry_tenant.as_str()),
        ("app_version", record.app_version.as_str()),
        ("bundle_digest", record.bundle_digest.as_str()),
        ("spec_digest", record.spec_digest.as_str()),
        ("policy_digest", record.policy_digest.as_str()),
        ("wasm_digest", record.wasm_digest.as_str()),
        ("content_digest", record.content_digest.as_str()),
        ("seed_digest", record.seed_digest.as_str()),
        ("status", record.status.as_str()),
    ] {
        components.push((format!("installed_app:{name}"), value.as_bytes().to_vec()));
    }
}
