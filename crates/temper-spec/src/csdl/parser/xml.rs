use quick_xml::Reader;
use quick_xml::events::{BytesEnd, BytesStart};

use super::CsdlParseError;

pub(super) fn skip_element(reader: &mut Reader<&[u8]>) -> Result<(), CsdlParseError> {
    let mut depth: u32 = 1;
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(quick_xml::events::Event::Start(_)) => depth += 1,
            Ok(quick_xml::events::Event::End(_)) => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            Ok(quick_xml::events::Event::Eof) => break,
            Err(error) => return Err(CsdlParseError::Xml(error)),
            _ => {}
        }
        buf.clear();
    }

    Ok(())
}

pub(super) fn local_name(element: &BytesStart) -> String {
    let name = element.name();
    let full = std::str::from_utf8(name.as_ref()).unwrap_or("");
    full.rsplit(':').next().unwrap_or(full).to_string()
}

pub(super) fn local_name_end(element: &BytesEnd) -> String {
    let name = element.name();
    let full = std::str::from_utf8(name.as_ref()).unwrap_or("");
    full.rsplit(':').next().unwrap_or(full).to_string()
}

pub(super) fn attr_str(element: &BytesStart, name: &str) -> Option<String> {
    element
        .attributes()
        .flatten()
        .find(|attribute| std::str::from_utf8(attribute.key.as_ref()).unwrap_or("") == name)
        .and_then(|attribute| String::from_utf8(attribute.value.to_vec()).ok())
}

pub(super) fn required_attr(element: &BytesStart, name: &str) -> Result<String, CsdlParseError> {
    attr_str(element, name).ok_or_else(|| CsdlParseError::MissingAttribute {
        element: local_name(element),
        attr: name.to_string(),
    })
}
