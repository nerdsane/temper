//! Deterministic canonical-to-Rust naming.

use std::collections::BTreeMap;

use super::ModuleSdkCodegenError;

pub(super) fn rust_type_name(name: &str) -> String {
    name.split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            chars
                .next()
                .map(|first| first.to_ascii_uppercase())
                .into_iter()
                .chain(chars)
                .collect::<String>()
        })
        .collect()
}

pub(super) fn rust_field_name(name: &str) -> String {
    let mut result = String::new();
    for (index, character) in name.chars().enumerate() {
        if character.is_ascii_alphanumeric() {
            if character.is_ascii_uppercase() && index > 0 && !result.ends_with('_') {
                result.push('_');
            }
            result.push(character.to_ascii_lowercase());
        } else if !result.ends_with('_') {
            result.push('_');
        }
    }
    if is_rust_keyword(&result) {
        result.push('_');
    }
    result
}

pub(super) fn rust_scalar_type(type_name: &str) -> &'static str {
    match type_name {
        "Edm.Boolean" => "bool",
        "Edm.Byte" | "Edm.Int16" | "Edm.Int32" | "Edm.Int64" => "i64",
        "Edm.Single" | "Edm.Double" => "f64",
        _ => "String",
    }
}

fn is_rust_keyword(value: &str) -> bool {
    matches!(
        value,
        "as" | "break"
            | "const"
            | "continue"
            | "crate"
            | "else"
            | "enum"
            | "extern"
            | "false"
            | "fn"
            | "for"
            | "if"
            | "impl"
            | "in"
            | "let"
            | "loop"
            | "match"
            | "mod"
            | "move"
            | "mut"
            | "pub"
            | "ref"
            | "return"
            | "self"
            | "Self"
            | "static"
            | "struct"
            | "super"
            | "trait"
            | "true"
            | "type"
            | "unsafe"
            | "use"
            | "where"
            | "while"
            | "async"
            | "await"
            | "dyn"
            | "gen"
    )
}

pub(super) fn reject_generated_collisions<'a>(
    entity_type: &str,
    names: impl Iterator<Item = (&'a String, &'a String)>,
) -> Result<(), ModuleSdkCodegenError> {
    let mut generated = BTreeMap::<&str, Vec<&str>>::new();
    for (canonical, rust_name) in names {
        generated.entry(rust_name).or_default().push(canonical);
    }
    if let Some((rust_name, canonical)) = generated.into_iter().find(|(_, names)| names.len() > 1) {
        return Err(ModuleSdkCodegenError::IdentifierCollision(format!(
            "{entity_type}: {rust_name} from {}",
            canonical.join(", ")
        )));
    }
    Ok(())
}
