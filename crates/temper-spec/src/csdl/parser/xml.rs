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

pub(super) fn local_name<'a>(element: &'a BytesStart<'_>) -> &'a str {
    let name = element.name();
    let full = std::str::from_utf8(name.into_inner()).unwrap_or("");
    full.rsplit(':').next().unwrap_or(full)
}

pub(super) fn local_name_end<'a>(element: &'a BytesEnd<'_>) -> &'a str {
    let name = element.name();
    let full = std::str::from_utf8(name.into_inner()).unwrap_or("");
    full.rsplit(':').next().unwrap_or(full)
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
        element: local_name(element).to_string(),
        attr: name.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use quick_xml::events::Event;

    #[test]
    fn local_names_borrow_start_and_end_names_after_the_last_colon() {
        for (name, expected) in [
            ("Schema", "Schema"),
            ("edm:Schema", "Schema"),
            ("vendor:edm:Schema", "Schema"),
            ("edm:", ""),
        ] {
            let start = BytesStart::new(name);
            let end = BytesEnd::new(name);
            assert_eq!(local_name(&start), expected);
            assert_eq!(local_name_end(&end), expected);
            let suffix = name.rsplit(':').next().unwrap();
            assert_eq!(local_name(&start).as_ptr(), suffix.as_ptr());
            assert_eq!(local_name_end(&end).as_ptr(), suffix.as_ptr());
        }
    }

    #[test]
    fn missing_attribute_diagnostic_owns_the_local_element_name() {
        let error = {
            let element = BytesStart::new(String::from("vendor:edm:Schema"));
            required_attr(&element, "Namespace").unwrap_err()
        };
        assert!(matches!(
            &error,
            CsdlParseError::MissingAttribute { element, attr }
                if element == "Schema" && attr == "Namespace"
        ));
        assert_eq!(
            error.to_string(),
            "missing required attribute 'Namespace' on element 'Schema'"
        );
    }

    #[test]
    fn invalid_utf8_names_keep_the_empty_string_fallback() {
        let mut reader = Reader::from_reader(&b"<\xff:Schema></\xff:Schema>"[..]);
        let Event::Start(start) = reader.read_event().unwrap() else {
            panic!("expected a start element");
        };
        assert_eq!(local_name(&start), "");
        let Event::End(end) = reader.read_event().unwrap() else {
            panic!("expected an end element");
        };
        assert_eq!(local_name_end(&end), "");
    }
}
