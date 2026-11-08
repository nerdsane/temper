//! Input checks for `temper verify`.
use anyhow::{Context, Result};
use std::{collections::BTreeMap, fs, path::PathBuf};
use temper_spec::csdl::CsdlDocument;

/// Require declared IOA entities to have CSDL types and correctly qualified sets.
/// Resolve declared schema aliases to namespaces before matching type references.
/// Also reject dangling entity-set references, even if another set is valid.
pub(super) fn validate_ioa_entities(
    csdl: &CsdlDocument,
    sources: &BTreeMap<String, String>,
) -> Result<()> {
    let mut qualifiers = BTreeMap::new();
    for schema in &csdl.schemas {
        for qualifier in std::iter::once(&schema.namespace).chain(schema.alias.iter()) {
            if let Some(existing) = qualifiers.insert(qualifier.as_str(), schema.namespace.as_str())
            {
                anyhow::ensure!(
                    existing == schema.namespace,
                    "ambiguous CSDL namespace or alias {qualifier}"
                );
            }
        }
    }
    let resolve_type = |name: &str| {
        name.rsplit_once('.')
            .and_then(|(qualifier, local_name)| {
                qualifiers
                    .get(qualifier)
                    .map(|namespace| format!("{namespace}.{local_name}"))
            })
            .unwrap_or_else(|| name.to_owned())
    };
    let declared_types: std::collections::BTreeSet<String> = csdl
        .schemas
        .iter()
        .flat_map(|schema| {
            schema
                .entity_types
                .iter()
                .map(|entity| format!("{}.{}", schema.namespace, entity.name))
        })
        .collect();
    let sets: Vec<_> = csdl
        .schemas
        .iter()
        .flat_map(|schema| &schema.entity_containers)
        .flat_map(|container| &container.entity_sets)
        .map(|set| (set, resolve_type(&set.entity_type)))
        .collect();
    for name in sources.keys() {
        let matching_types: Vec<_> = csdl
            .schemas
            .iter()
            .filter(|schema| {
                schema
                    .entity_types
                    .iter()
                    .any(|entity| entity.name == *name)
            })
            .map(|schema| format!("{}.{name}", schema.namespace))
            .collect();
        anyhow::ensure!(
            !matching_types.is_empty(),
            "IOA entity {name} is missing from CSDL"
        );
        anyhow::ensure!(
            sets.iter()
                .any(|(_, resolved_type)| matching_types.contains(resolved_type)),
            "IOA entity {name} has no CSDL entity set referencing its declared type"
        );
    }
    for (set, resolved_type) in sets {
        anyhow::ensure!(
            declared_types.contains(&resolved_type),
            "CSDL entity set {} references undeclared type {}",
            set.name,
            set.entity_type
        );
    }
    Ok(())
}

/// Read each IOA spec by its automaton name; two specs of one name fail.
pub(super) fn read_ioa_files(paths: &[PathBuf]) -> Result<BTreeMap<String, String>> {
    let mut sources = BTreeMap::new();
    for path in paths {
        let source = fs::read_to_string(path)
            .with_context(|| format!("Failed to read IOA file: {}", path.display()))?;
        let entity_name = temper_spec::automaton::parse_automaton(&source)?
            .automaton
            .name;
        anyhow::ensure!(
            sources.insert(entity_name.clone(), source).is_none(),
            "duplicate entity {entity_name}"
        );
    }
    Ok(sources)
}

/// Reject truncated XML and unrelated XML documents before semantic verification.
pub(super) fn validate_xml_document(xml: &str) -> Result<()> {
    use quick_xml::{Reader, events::Event};
    let mut reader = Reader::from_str(xml);
    let mut depth = 0usize;
    let mut roots = 0usize;
    loop {
        match reader.read_event().context("malformed CSDL XML")? {
            Event::Start(element) => {
                if depth == 0 {
                    roots += 1;
                    anyhow::ensure!(
                        element.local_name().as_ref() == b"Edmx",
                        "CSDL root must be Edmx"
                    );
                }
                depth += 1;
            }
            Event::Empty(element) if depth == 0 => {
                roots += 1;
                anyhow::ensure!(
                    element.local_name().as_ref() == b"Edmx",
                    "CSDL root must be Edmx"
                );
            }
            Event::End(_) => {
                depth = depth.checked_sub(1).context("unbalanced CSDL XML")?;
            }
            Event::DocType(_) => anyhow::bail!("CSDL document types are not supported"),
            Event::Eof => break,
            _ => {}
        }
    }
    anyhow::ensure!(
        depth == 0 && roots == 1,
        "CSDL must be one complete XML document"
    );
    Ok(())
}
