//! Serialize a [`CsdlDocument`] back to OData CSDL XML.

use super::types::*;

/// Serialize a [`CsdlDocument`] to an OData 4.0 CSDL XML string.
///
/// The output is a valid `edmx:Edmx` document that can be served as `$metadata`
/// or round-tripped through [`parse_csdl`](super::parse_csdl).
pub fn emit_csdl_xml(doc: &CsdlDocument) -> String {
    let mut out = String::from("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n");
    out.push_str(&format!(
        "<edmx:Edmx Version=\"{}\" xmlns:edmx=\"http://docs.oasis-open.org/odata/ns/edmx\">\n",
        xml_escape(&doc.version)
    ));
    out.push_str("  <edmx:DataServices>\n");

    for schema in &doc.schemas {
        emit_schema(&mut out, schema);
    }

    out.push_str("  </edmx:DataServices>\n");
    out.push_str("</edmx:Edmx>\n");
    out
}

fn emit_schema(out: &mut String, schema: &Schema) {
    out.push_str(&format!(
        "    <Schema Namespace=\"{}\" xmlns=\"http://docs.oasis-open.org/odata/ns/edm\">\n",
        xml_escape(&schema.namespace)
    ));

    for term in &schema.terms {
        emit_term(out, term);
    }
    for enum_type in &schema.enum_types {
        emit_enum_type(out, enum_type);
    }
    for entity_type in &schema.entity_types {
        emit_entity_type(out, entity_type);
    }
    for action in &schema.actions {
        emit_action(out, action);
    }
    for function in &schema.functions {
        emit_function(out, function);
    }
    for container in &schema.entity_containers {
        emit_entity_container(out, container);
    }

    out.push_str("    </Schema>\n");
}

fn emit_term(out: &mut String, term: &Term) {
    out.push_str(&format!(
        "      <Term Name=\"{}\" Type=\"{}\"",
        xml_escape(&term.name),
        xml_escape(&term.type_name)
    ));
    if let Some(ref applies_to) = term.applies_to {
        out.push_str(&format!(" AppliesTo=\"{}\"", xml_escape(applies_to)));
    }
    if let Some(ref description) = term.description {
        out.push_str(&format!(" Description=\"{}\"", xml_escape(description)));
    }
    out.push_str("/>\n");
}

fn emit_enum_type(out: &mut String, et: &EnumType) {
    out.push_str(&format!(
        "      <EnumType Name=\"{}\">\n",
        xml_escape(&et.name)
    ));
    for member in &et.members {
        if let Some(val) = member.value {
            out.push_str(&format!(
                "        <Member Name=\"{}\" Value=\"{}\"/>\n",
                xml_escape(&member.name),
                val
            ));
        } else {
            out.push_str(&format!(
                "        <Member Name=\"{}\"/>\n",
                xml_escape(&member.name)
            ));
        }
    }
    out.push_str("      </EnumType>\n");
}

fn emit_entity_type(out: &mut String, et: &EntityType) {
    if et.has_stream {
        out.push_str(&format!(
            "      <EntityType Name=\"{}\" HasStream=\"true\">\n",
            xml_escape(&et.name)
        ));
    } else {
        out.push_str(&format!(
            "      <EntityType Name=\"{}\">\n",
            xml_escape(&et.name)
        ));
    }

    // Key
    if !et.key_properties.is_empty() {
        out.push_str("        <Key>\n");
        for key in &et.key_properties {
            out.push_str(&format!(
                "          <PropertyRef Name=\"{}\"/>\n",
                xml_escape(key)
            ));
        }
        out.push_str("        </Key>\n");
    }

    // Properties
    for prop in &et.properties {
        emit_property(out, prop);
    }

    // Navigation properties
    for nav in &et.navigation_properties {
        emit_navigation_property(out, nav);
    }

    // Annotations
    for ann in &et.annotations {
        emit_annotation(out, ann, 8);
    }

    out.push_str("      </EntityType>\n");
}

fn emit_property(out: &mut String, prop: &Property) {
    out.push_str(&format!(
        "        <Property Name=\"{}\" Type=\"{}\"",
        xml_escape(&prop.name),
        xml_escape(&prop.type_name)
    ));
    if !prop.nullable {
        out.push_str(" Nullable=\"false\"");
    }
    if let Some(ref default) = prop.default_value {
        out.push_str(&format!(" DefaultValue=\"{}\"", xml_escape(default)));
    }
    if let Some(precision) = prop.precision {
        out.push_str(&format!(" Precision=\"{precision}\""));
    }
    if let Some(scale) = prop.scale {
        out.push_str(&format!(" Scale=\"{scale}\""));
    }
    out.push_str("/>\n");
}

fn emit_navigation_property(out: &mut String, nav: &NavigationProperty) {
    let has_children = !nav.referential_constraints.is_empty();

    out.push_str(&format!(
        "        <NavigationProperty Name=\"{}\" Type=\"{}\"",
        xml_escape(&nav.name),
        xml_escape(&nav.type_name)
    ));
    if !nav.nullable {
        out.push_str(" Nullable=\"false\"");
    }
    if nav.contains_target {
        out.push_str(" ContainsTarget=\"true\"");
    }

    if has_children {
        out.push_str(">\n");
        for rc in &nav.referential_constraints {
            out.push_str(&format!(
                "          <ReferentialConstraint Property=\"{}\" ReferencedProperty=\"{}\"/>\n",
                xml_escape(&rc.property),
                xml_escape(&rc.referenced_property)
            ));
        }
        out.push_str("        </NavigationProperty>\n");
    } else {
        out.push_str("/>\n");
    }
}

fn emit_action(out: &mut String, action: &Action) {
    let has_children = !action.parameters.is_empty()
        || action.return_type.is_some()
        || !action.annotations.is_empty();

    out.push_str(&format!(
        "      <Action Name=\"{}\"",
        xml_escape(&action.name)
    ));
    if action.is_bound {
        out.push_str(" IsBound=\"true\"");
    }

    if has_children {
        out.push_str(">\n");
        for param in &action.parameters {
            emit_parameter(out, param);
        }
        if let Some(ref rt) = action.return_type {
            emit_return_type(out, rt);
        }
        for ann in &action.annotations {
            emit_annotation(out, ann, 8);
        }
        out.push_str("      </Action>\n");
    } else {
        out.push_str("/>\n");
    }
}

fn emit_function(out: &mut String, func: &Function) {
    let has_children =
        !func.parameters.is_empty() || func.return_type.is_some() || !func.annotations.is_empty();

    out.push_str(&format!(
        "      <Function Name=\"{}\"",
        xml_escape(&func.name)
    ));
    if func.is_bound {
        out.push_str(" IsBound=\"true\"");
    }

    if has_children {
        out.push_str(">\n");
        for param in &func.parameters {
            emit_parameter(out, param);
        }
        if let Some(ref rt) = func.return_type {
            emit_return_type(out, rt);
        }
        for ann in &func.annotations {
            emit_annotation(out, ann, 8);
        }
        out.push_str("      </Function>\n");
    } else {
        out.push_str("/>\n");
    }
}

fn emit_parameter(out: &mut String, param: &Parameter) {
    out.push_str(&format!(
        "        <Parameter Name=\"{}\" Type=\"{}\"",
        xml_escape(&param.name),
        xml_escape(&param.type_name)
    ));
    if !param.nullable {
        out.push_str(" Nullable=\"false\"");
    }
    if let Some(ref default) = param.default_value {
        out.push_str(&format!(" DefaultValue=\"{}\"", xml_escape(default)));
    }
    out.push_str("/>\n");
}

fn emit_return_type(out: &mut String, rt: &ReturnType) {
    out.push_str(&format!(
        "        <ReturnType Type=\"{}\"",
        xml_escape(&rt.type_name)
    ));
    if !rt.nullable {
        out.push_str(" Nullable=\"false\"");
    }
    if let Some(precision) = rt.precision {
        out.push_str(&format!(" Precision=\"{precision}\""));
    }
    if let Some(scale) = rt.scale {
        out.push_str(&format!(" Scale=\"{scale}\""));
    }
    out.push_str("/>\n");
}

fn emit_entity_container(out: &mut String, container: &EntityContainer) {
    let has_children = !container.entity_sets.is_empty()
        || !container.action_imports.is_empty()
        || !container.function_imports.is_empty();

    out.push_str(&format!(
        "      <EntityContainer Name=\"{}\"",
        xml_escape(&container.name)
    ));

    if has_children {
        out.push_str(">\n");
        for es in &container.entity_sets {
            emit_entity_set(out, es);
        }
        for ai in &container.action_imports {
            out.push_str(&format!(
                "        <ActionImport Name=\"{}\" Action=\"{}\"/>\n",
                xml_escape(&ai.name),
                xml_escape(&ai.action)
            ));
        }
        for fi in &container.function_imports {
            out.push_str(&format!(
                "        <FunctionImport Name=\"{}\" Function=\"{}\"/>\n",
                xml_escape(&fi.name),
                xml_escape(&fi.function)
            ));
        }
        out.push_str("      </EntityContainer>\n");
    } else {
        out.push_str("/>\n");
    }
}

fn emit_entity_set(out: &mut String, es: &EntitySet) {
    if es.navigation_bindings.is_empty() {
        out.push_str(&format!(
            "        <EntitySet Name=\"{}\" EntityType=\"{}\"/>\n",
            xml_escape(&es.name),
            xml_escape(&es.entity_type)
        ));
    } else {
        out.push_str(&format!(
            "        <EntitySet Name=\"{}\" EntityType=\"{}\">\n",
            xml_escape(&es.name),
            xml_escape(&es.entity_type)
        ));
        for nb in &es.navigation_bindings {
            out.push_str(&format!(
                "          <NavigationPropertyBinding Path=\"{}\" Target=\"{}\"/>\n",
                xml_escape(&nb.path),
                xml_escape(&nb.target)
            ));
        }
        out.push_str("        </EntitySet>\n");
    }
}

fn emit_annotation(out: &mut String, ann: &Annotation, indent: usize) {
    let pad: String = " ".repeat(indent);
    let term = xml_escape(&ann.term);
    match &ann.value {
        AnnotationValue::String(s) => {
            out.push_str(&format!(
                "{pad}<Annotation Term=\"{}\" String=\"{}\"/>\n",
                term,
                xml_escape(s)
            ));
        }
        AnnotationValue::Float(f) => {
            out.push_str(&format!(
                "{pad}<Annotation Term=\"{}\" Float=\"{f}\"/>\n",
                term
            ));
        }
        AnnotationValue::Bool(b) => {
            out.push_str(&format!(
                "{pad}<Annotation Term=\"{}\" Bool=\"{b}\"/>\n",
                term
            ));
        }
        AnnotationValue::Int(i) => {
            out.push_str(&format!(
                "{pad}<Annotation Term=\"{}\" Int=\"{i}\"/>\n",
                term
            ));
        }
        AnnotationValue::Collection(items) => {
            out.push_str(&format!("{pad}<Annotation Term=\"{term}\">\n"));
            out.push_str(&format!("{pad}  <Collection>\n"));
            for item in items {
                out.push_str(&format!(
                    "{pad}    <String>{}</String>\n",
                    xml_escape_text(item)
                ));
            }
            out.push_str(&format!("{pad}  </Collection>\n"));
            out.push_str(&format!("{pad}</Annotation>\n"));
        }
        AnnotationValue::Record(map) => {
            out.push_str(&format!("{pad}<Annotation Term=\"{term}\">\n"));
            out.push_str(&format!("{pad}  <Record>\n"));
            let mut entries = map.iter().collect::<Vec<_>>();
            entries.sort_by_key(|(key, _)| *key);
            for (k, v) in entries {
                out.push_str(&format!(
                    "{pad}    <PropertyValue Property=\"{}\" String=\"{}\"/>\n",
                    xml_escape(k),
                    xml_escape(v)
                ));
            }
            out.push_str(&format!("{pad}  </Record>\n"));
            out.push_str(&format!("{pad}</Annotation>\n"));
        }
    }
}

/// Escape XML special characters in attribute values.
///
/// Every value interpolated into emitted CSDL must pass through this function —
/// identifiers included. Names, types, and references are agent- or
/// user-influenced, so an unescaped `"` there closes the attribute and lets the
/// value inject arbitrary markup.
///
/// Tab, newline, and carriage return are escaped as character references
/// because XML attribute-value normalization would otherwise replace them with
/// spaces, silently changing the value on the way back in.
fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            '\t' => out.push_str("&#x9;"),
            '\n' => out.push_str("&#xA;"),
            '\r' => out.push_str("&#xD;"),
            _ => out.push(ch),
        }
    }
    out
}

/// Escape XML special characters in text nodes without changing whitespace.
///
/// Attribute-value normalization does not apply to text nodes, so preserving
/// literal tabs and newlines keeps collection values stable when parsed again.
fn xml_escape_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(ch),
        }
    }
    out
}

#[cfg(test)]
#[path = "emit_test.rs"]
mod tests;
