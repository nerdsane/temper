use std::path::Path;

use temper_wasm_sdk::data::ModuleSdkManifest;
use toml_edit::{DocumentMut, Item};

pub(super) fn with_module_binding(
    source: &str,
    module_name: &str,
    binding: &ModuleSdkManifest,
) -> Result<String, String> {
    let mut document = source
        .parse::<DocumentMut>()
        .map_err(|error| format!("failed to edit app manifest: {error}"))?;
    let modules = document
        .get_mut("wasm_modules")
        .and_then(Item::as_array_of_tables_mut)
        .ok_or_else(|| "app manifest has no [[wasm_modules]] declarations".to_string())?;
    let module = modules
        .iter_mut()
        .find(|table| table.get("name").and_then(Item::as_str) == Some(module_name))
        .ok_or_else(|| format!("module '{module_name}' is absent from app manifest"))?;
    let binding_item = toml_edit::ser::to_document(binding)
        .map_err(|error| format!("failed to encode module binding: {error}"))?
        .into_item();
    module.insert("data_binding", binding_item);
    Ok(document.to_string())
}

pub(super) fn read(path: &Path) -> Result<String, String> {
    std::fs::read_to_string(path)
        .map_err(|error| format!("failed to read app manifest '{}': {error}", path.display()))
}
