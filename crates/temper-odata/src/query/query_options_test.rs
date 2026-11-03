//! Query option values reach their parsers as the client sent them, after a
//! single percent-decode.

use super::*;
use crate::query::filter::parse_filter;

#[test]
fn a_raw_query_string_decodes_each_value_after_splitting() {
    for (raw, literal) in [
        ("Notes%20eq%20'R%26D'", "R&D"),
        ("Notes%20eq%20'a%20%28draft'", "a (draft"),
        ("Notes%20eq%20'x%3Dy'", "x=y"),
        ("Notes+eq+'50%25'", "50%"),
    ] {
        let options = parse_query_options(&format!("$filter={raw}&$top=10"))
            .unwrap_or_else(|error| panic!("{raw}: {error}"));
        let expected = parse_filter(&format!("Notes eq '{literal}'")).expect("expected filter");
        assert_eq!(options.filter, Some(expected), "{raw}");
        assert_eq!(options.top, Some(10), "{raw}");
    }
}

#[test]
fn decoded_pairs_reach_the_filter_parser_unchanged() {
    // `50%25` must stay as given: pairs are already decoded and are not decoded again.
    for literal in ["R&D", "a (draft", "b) c", "x=y", "50%25"] {
        let filter = format!("Notes eq '{literal}'");
        let options = parse_query_options_from_pairs([
            ("$filter", filter.as_str()),
            ("$top", "10"),
            ("$orderby", "Notes desc"),
        ])
        .unwrap_or_else(|error| panic!("{literal}: {error}"));
        let expected = parse_filter(&filter).expect("expected filter");
        assert_eq!(options.filter, Some(expected), "{literal}");
        assert_eq!(options.top, Some(10), "{literal}");
        assert_eq!(
            options.orderby,
            Some(vec![OrderByClause {
                property: "Notes".into(),
                direction: OrderDirection::Desc,
            }]),
            "{literal}"
        );
    }
}
