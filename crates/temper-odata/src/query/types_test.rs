//! Parser tests for each system query option.

use super::*;

// -- $select tests -------------------------------------------------------

#[test]
fn select_multiple_properties() {
    let opts = parse_query_options("$select=Id,Name,Total").unwrap();
    assert_eq!(
        opts.select,
        Some(vec!["Id".into(), "Name".into(), "Total".into()])
    );
}

// -- $orderby tests ------------------------------------------------------

#[test]
fn orderby_multiple_clauses() {
    let opts = parse_query_options("$orderby=CreatedAt desc,Name asc").unwrap();
    assert_eq!(
        opts.orderby,
        Some(vec![
            OrderByClause {
                property: "CreatedAt".into(),
                direction: OrderDirection::Desc,
            },
            OrderByClause {
                property: "Name".into(),
                direction: OrderDirection::Asc,
            },
        ])
    );
}

#[test]
fn orderby_default_direction_is_asc() {
    let opts = parse_query_options("$orderby=Name").unwrap();
    assert_eq!(
        opts.orderby,
        Some(vec![OrderByClause {
            property: "Name".into(),
            direction: OrderDirection::Asc,
        }])
    );
}

// -- $top, $skip, $count tests -------------------------------------------

#[test]
fn top_skip_count() {
    let opts = parse_query_options("$top=10&$skip=20&$count=true").unwrap();
    assert_eq!(opts.top, Some(10));
    assert_eq!(opts.skip, Some(20));
    assert_eq!(opts.count, Some(true));
}

// -- $expand tests -------------------------------------------------------

#[test]
fn expand_simple() {
    let opts = parse_query_options("$expand=Items").unwrap();
    assert_eq!(
        opts.expand,
        Some(vec![ExpandItem {
            property: "Items".into(),
            options: None,
        }])
    );
}

#[test]
fn expand_with_nested_options() {
    let opts =
        parse_query_options("$expand=Items($select=Id,Quantity;$orderby=Quantity desc)").unwrap();
    let expand = opts.expand.unwrap();
    assert_eq!(expand.len(), 1);
    assert_eq!(expand[0].property, "Items");
    let nested = expand[0].options.as_ref().unwrap();
    assert_eq!(nested.select, Some(vec!["Id".into(), "Quantity".into()]));
    assert_eq!(
        nested.orderby,
        Some(vec![OrderByClause {
            property: "Quantity".into(),
            direction: OrderDirection::Desc,
        }])
    );
}

#[test]
fn expand_multiple_items() {
    let opts = parse_query_options("$expand=Items,Customer").unwrap();
    let expand = opts.expand.unwrap();
    assert_eq!(expand.len(), 2);
    assert_eq!(expand[0].property, "Items");
    assert_eq!(expand[1].property, "Customer");
}

// -- Combined query test -------------------------------------------------

#[test]
fn combined_query_options() {
    let opts = parse_query_options(
        "$filter=Status eq 'Active'&$select=Id,Name&$orderby=Name asc&$top=50&$skip=0&$count=true",
    )
    .unwrap();

    assert!(opts.filter.is_some());
    assert_eq!(opts.select, Some(vec!["Id".into(), "Name".into()]));
    assert_eq!(
        opts.orderby,
        Some(vec![OrderByClause {
            property: "Name".into(),
            direction: OrderDirection::Asc,
        }])
    );
    assert_eq!(opts.top, Some(50));
    assert_eq!(opts.skip, Some(0));
    assert_eq!(opts.count, Some(true));
}

#[test]
fn empty_query_string() {
    let opts = parse_query_options("").unwrap();
    assert_eq!(opts, QueryOptions::default());
}

#[test]
fn skiptoken_is_parsed_and_blank_is_ignored() {
    let opts = parse_query_options("$filter=Status eq 'Published'&$skiptoken=Zm9vLTAx").unwrap();
    assert_eq!(opts.skiptoken.as_deref(), Some("Zm9vLTAx"));

    let blank = parse_query_options("$skiptoken=").unwrap();
    assert_eq!(blank.skiptoken, None);
}

#[test]
fn invalid_top_value() {
    let result = parse_query_options("$top=abc");
    assert!(result.is_err());
}
