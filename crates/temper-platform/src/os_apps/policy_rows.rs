use super::AppBundle;

pub(super) struct OwnedPolicyEntry {
    pub(super) policy_id: String,
    pub(super) cedar_text: String,
    pub(super) created_by: String,
}

pub(crate) fn os_app_policy_row_id(app_name: &str, relative_path: &str) -> String {
    fn hex_component(value: &str) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut encoded = String::with_capacity(value.len() * 2);
        for byte in value.as_bytes() {
            encoded.push(char::from(HEX[usize::from(byte >> 4)]));
            encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
        encoded
    }

    // Hex encoding is injective and the separator is outside its alphabet, so
    // no app owner/path pair can alias another row in the tenant-wide primary
    // key. Slug concatenation was ambiguous across both owner and path.
    format!(
        "os-app-{}-{}",
        hex_component(app_name),
        hex_component(relative_path.trim_start_matches('/'))
    )
}

pub(super) fn bundle_policy_entries(app_name: &str, bundle: &AppBundle) -> Vec<OwnedPolicyEntry> {
    let created_by = format!("os-app:{app_name}");
    let mut entries = Vec::new();
    for source in &bundle.cedar_policy_sources {
        let cedar_text = source.text.trim();
        if cedar_text.is_empty() {
            continue;
        }
        entries.push(OwnedPolicyEntry {
            policy_id: os_app_policy_row_id(app_name, &source.relative_path),
            cedar_text: cedar_text.to_string(),
            created_by: created_by.clone(),
        });
    }
    entries
}
