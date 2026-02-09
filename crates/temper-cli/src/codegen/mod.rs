//! Code generation command for `temper codegen`.
//!
//! Reads CSDL and TLA+ specifications from the specs directory,
//! builds a unified spec model, and generates Rust entity modules.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use temper_codegen::generate_entity_module;
use temper_spec::csdl::parse_csdl;
use temper_spec::model::build_spec_model;


/// Run the `temper codegen` command.
///
/// Reads specs from `specs_dir`, generates Rust code, and writes to `output_dir`.
pub fn run(specs_dir: &str, output_dir: &str) -> Result<()> {
    let specs_path = Path::new(specs_dir);
    let output_path = Path::new(output_dir);

    println!("Running code generation...");
    println!("  Specs directory: {}", specs_path.display());
    println!("  Output directory: {}", output_path.display());

    // Read the CSDL model file
    let csdl_path = specs_path.join("model.csdl.xml");
    if !csdl_path.exists() {
        anyhow::bail!(
            "CSDL model file not found at {}. Run `temper init` first.",
            csdl_path.display()
        );
    }

    let csdl_xml = fs::read_to_string(&csdl_path)
        .with_context(|| format!("Failed to read {}", csdl_path.display()))?;
    println!("  Read CSDL from {}", csdl_path.display());

    // Parse CSDL
    let csdl = parse_csdl(&csdl_xml)
        .with_context(|| format!("Failed to parse CSDL from {}", csdl_path.display()))?;
    println!(
        "  Parsed {} schema(s) from CSDL",
        csdl.schemas.len()
    );

    // Read TLA+ spec files
    let tla_sources = read_tla_sources(specs_path)?;
    if tla_sources.is_empty() {
        println!("  No TLA+ spec files found (state machines will be skipped)");
    } else {
        println!("  Found {} TLA+ spec file(s)", tla_sources.len());
    }

    // Build the unified spec model
    let spec = build_spec_model(csdl, tla_sources);

    // Report validation results
    if !spec.validation.errors.is_empty() {
        println!("\n  Validation errors:");
        for err in &spec.validation.errors {
            println!("    ERROR: {err}");
        }
    }
    if !spec.validation.warnings.is_empty() {
        println!("\n  Validation warnings:");
        for warn in &spec.validation.warnings {
            println!("    WARN: {warn}");
        }
    }

    if !spec.validation.is_valid() {
        anyhow::bail!(
            "Specification validation failed with {} error(s). Fix errors before generating code.",
            spec.validation.errors.len()
        );
    }

    // Create output directory
    fs::create_dir_all(output_path)
        .with_context(|| format!("Failed to create output directory: {}", output_path.display()))?;

    // Collect all entity type names from non-vocabulary schemas
    let entity_names: Vec<String> = spec
        .csdl
        .schemas
        .iter()
        .filter(|s| !s.entity_types.is_empty())
        .flat_map(|s| s.entity_types.iter().map(|e| e.name.clone()))
        .collect();

    if entity_names.is_empty() {
        println!("\n  No entity types found in CSDL. Nothing to generate.");
        return Ok(());
    }

    println!("\n  Generating code for {} entity type(s)...", entity_names.len());

    let mut generated_count = 0;
    let mut mod_entries = Vec::new();

    for entity_name in &entity_names {
        match generate_entity_module(&spec, entity_name) {
            Ok(module) => {
                let file_name = to_snake_case(&module.entity_name);
                let file_path = output_path.join(format!("{file_name}.rs"));

                fs::write(&file_path, &module.source).with_context(|| {
                    format!("Failed to write generated file: {}", file_path.display())
                })?;

                println!("    Generated {}", file_path.display());
                mod_entries.push(file_name);
                generated_count += 1;
            }
            Err(e) => {
                println!("    Skipped {entity_name}: {e}");
            }
        }
    }

    // Write a mod.rs to re-export all generated modules
    if !mod_entries.is_empty() {
        let mod_content = mod_entries
            .iter()
            .map(|name| format!("pub mod {name};"))
            .collect::<Vec<_>>()
            .join("\n");
        let mod_path = output_path.join("mod.rs");
        fs::write(&mod_path, format!("//! Generated entity modules.\n//! DO NOT EDIT -- regenerate from specs with `temper codegen`.\n\n{mod_content}\n"))
            .with_context(|| format!("Failed to write {}", mod_path.display()))?;
        println!("    Generated {}", mod_path.display());
    }

    println!("\nCode generation complete: {generated_count} module(s) generated.");
    Ok(())
}

/// Read all `.tla` files from the specs directory and return a map of
/// entity name (derived from file stem, PascalCase) to TLA+ source text.
fn read_tla_sources(specs_dir: &Path) -> Result<HashMap<String, String>> {
    let mut sources = HashMap::new();

    if !specs_dir.is_dir() {
        return Ok(sources);
    }

    for entry in fs::read_dir(specs_dir)
        .with_context(|| format!("Failed to read specs directory: {}", specs_dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();

        if path.extension().and_then(|e| e.to_str()) == Some("tla") {
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default();

            // Convert file stem to PascalCase entity name
            let entity_name = to_pascal_case(stem);
            let source = fs::read_to_string(&path)
                .with_context(|| format!("Failed to read TLA+ file: {}", path.display()))?;

            println!("  Read TLA+ spec: {} -> entity '{}'", path.display(), entity_name);
            sources.insert(entity_name, source);
        }
    }

    Ok(sources)
}

/// Convert a string to PascalCase.
///
/// "order" -> "Order", "order_item" -> "OrderItem"
fn to_pascal_case(s: &str) -> String {
    s.split(|c: char| c == '_' || c == '-')
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => {
                    let upper: String = first.to_uppercase().collect();
                    format!("{}{}", upper, chars.collect::<String>())
                }
                None => String::new(),
            }
        })
        .collect()
}

/// Convert a PascalCase or camelCase string to snake_case.
///
/// "Order" -> "order", "OrderItem" -> "order_item"
fn to_snake_case(s: &str) -> String {
    let mut result = String::new();
    for (i, c) in s.chars().enumerate() {
        if c.is_uppercase() {
            if i > 0 {
                result.push('_');
            }
            result.extend(c.to_lowercase());
        } else {
            result.push(c);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_to_pascal_case() {
        assert_eq!(to_pascal_case("order"), "Order");
        assert_eq!(to_pascal_case("order_item"), "OrderItem");
        assert_eq!(to_pascal_case("my-entity"), "MyEntity");
    }

    #[test]
    fn test_to_snake_case() {
        assert_eq!(to_snake_case("Order"), "order");
        assert_eq!(to_snake_case("OrderItem"), "order_item");
        assert_eq!(to_snake_case("Customer"), "customer");
    }

    #[test]
    fn test_codegen_from_reference_specs() {
        // Use the example specs that ship with the project
        let specs_dir = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../test-fixtures/specs"
        );
        let specs_path = Path::new(specs_dir);

        // Verify the reference specs exist before testing
        if !specs_path.join("model.csdl.xml").exists() {
            // If reference specs don't exist, skip (don't fail CI)
            eprintln!("Skipping codegen test: reference specs not found at {}", specs_dir);
            return;
        }

        // Create a temp output directory
        let tmp = std::env::temp_dir().join(format!(
            "temper_test_codegen_{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&tmp);

        let result = run(specs_dir, tmp.to_str().unwrap());
        result.expect("codegen should succeed on reference specs");

        // Verify output files were created
        assert!(tmp.join("mod.rs").is_file(), "mod.rs should be generated");
        assert!(
            tmp.join("order.rs").is_file(),
            "order.rs should be generated"
        );
        assert!(
            tmp.join("customer.rs").is_file(),
            "customer.rs should be generated"
        );
        assert!(
            tmp.join("product.rs").is_file(),
            "product.rs should be generated"
        );

        // Verify order.rs content has key structures
        let order_src = fs::read_to_string(tmp.join("order.rs")).unwrap();
        assert!(
            order_src.contains("pub struct OrderState"),
            "should contain OrderState struct"
        );
        assert!(
            order_src.contains("pub enum OrderStatus"),
            "should contain OrderStatus enum"
        );
        assert!(
            order_src.contains("pub enum OrderMsg"),
            "should contain OrderMsg enum"
        );

        // Verify mod.rs content
        let mod_src = fs::read_to_string(tmp.join("mod.rs")).unwrap();
        assert!(mod_src.contains("pub mod order;"));
        assert!(mod_src.contains("pub mod customer;"));

        // Clean up
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_codegen_fails_without_csdl() {
        let tmp = std::env::temp_dir().join(format!(
            "temper_test_codegen_no_csdl_{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        let result = run(tmp.to_str().unwrap(), tmp.join("out").to_str().unwrap());
        assert!(result.is_err());
        assert!(
            result.unwrap_err().to_string().contains("CSDL model file not found"),
            "should report missing CSDL"
        );

        let _ = fs::remove_dir_all(&tmp);
    }
}
