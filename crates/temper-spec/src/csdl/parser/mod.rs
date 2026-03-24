mod elements;
mod schema;
mod xml;

use quick_xml::Reader;
use quick_xml::events::Event;

use super::types::*;
use schema::parse_schema;
use xml::{attr_str, local_name};

#[derive(Debug, thiserror::Error)]
pub enum CsdlParseError {
    #[error("XML parse error: {0}")]
    Xml(#[from] quick_xml::Error),
    #[error("missing required attribute '{attr}' on element '{element}'")]
    MissingAttribute { element: String, attr: String },
    #[error("unexpected element: {0}")]
    UnexpectedElement(String),
    #[error("invalid CSDL: {0}")]
    Invalid(String),
}

/// Parse a CSDL XML document from a string.
pub fn parse_csdl(xml: &str) -> Result<CsdlDocument, CsdlParseError> {
    let mut reader = Reader::from_str(xml);
    let mut doc = CsdlDocument {
        version: String::new(),
        schemas: Vec::new(),
    };

    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => match local_name(e).as_str() {
                "Edmx" => doc.version = attr_str(e, "Version").unwrap_or_default(),
                "Schema" => doc.schemas.push(parse_schema(&mut reader, e)?),
                _ => {}
            },
            Ok(Event::Empty(ref e)) => {
                if local_name(e) == "Edmx" {
                    doc.version = attr_str(e, "Version").unwrap_or_default();
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(CsdlParseError::Xml(e)),
            _ => {}
        }
        buf.clear();
    }
    Ok(doc)
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
