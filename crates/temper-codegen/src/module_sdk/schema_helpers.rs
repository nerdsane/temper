use std::collections::BTreeMap;

use temper_spec::csdl::CsdlDocument;

pub(super) fn entity_sets(csdl: &CsdlDocument) -> BTreeMap<&str, &str> {
    csdl.schemas
        .iter()
        .flat_map(|schema| &schema.entity_containers)
        .flat_map(|container| &container.entity_sets)
        .map(|set| (set.entity_type.as_str(), set.name.as_str()))
        .collect()
}

pub(super) fn enum_members(csdl: &CsdlDocument, type_name: &str) -> Vec<String> {
    let Some((namespace, name)) = type_name.rsplit_once('.') else {
        return Vec::new();
    };
    csdl.schemas
        .iter()
        .find(|schema| schema.namespace == namespace)
        .and_then(|schema| schema.enum_type(name))
        .map(|enum_type| {
            let mut members = enum_type
                .members
                .iter()
                .map(|member| member.name.clone())
                .collect::<Vec<_>>();
            members.sort();
            members
        })
        .unwrap_or_default()
}
